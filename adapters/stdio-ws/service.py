#!/usr/bin/env python3
#++agent TASK-225 [17.10.2026 12:09:00]
"""Приватный owner CLI; MCP поверхность и доступ к native не расширяет."""
import argparse
import asyncio
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import sys
import time
import uuid

import policy
import provider

ROOT = Path(provider.STATE_DIR)
BOOT = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
class PermanentConfigError(Exception):
    pass


class RelayCleanupFailed(Exception):
    pass


SDK_KEYS = ('testpilot_command', 'testpilot_args', 'descriptor_sha256', 'server_version', 'profiles_file', 'profiles_sha256')


def identity(pid=None):
    pid = pid or os.getpid()
    return dict(pid=pid, process_birth=Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19], os_uid=os.stat(f'/proc/{pid}').st_uid, boot_id=BOOT)


def ensure_root():
    ROOT.mkdir(parents=True, exist_ok=True, mode=0o700)
    st = os.lstat(ROOT)
    if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid() or stat.S_IMODE(st.st_mode) != 0o700:
        raise provider.AdmissionError()
    # Открытие контрольного leaf дополнительно проходит no-follow ancestors.
    fd = os.open(ROOT / 'control.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    os.close(fd)
    with provider.private_open(str(ROOT / 'control.lock')):
        pass


@contextlib.contextmanager
def control():
    ensure_root()
    fd = os.open(ROOT / 'control.lock', os.O_RDWR | os.O_NOFOLLOW)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def read(path):
    return provider.FrozenReceipt(str(path), strict=True)


def atomic(name, value):
    ensure_root()
    target = ROOT / name
    if target.exists() or target.is_symlink():
        with provider.private_open(str(target)):
            pass
    temp = ROOT / ('.' + uuid.uuid4().hex)
    fd = os.open(temp, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, separators=(',', ':'))
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, target)
    finally:
        with contextlib.suppress(FileNotFoundError):
            temp.unlink()


def paused():
    try:
        return read(ROOT / 'pause.json').data.get('boot_id') == BOOT
    except FileNotFoundError:
        return False


def health():
    pause = False
    try:
        pause = paused()
        data = read(ROOT / 'health.json').data
        actual = identity(data['pid'])
        if any(data.get(k) != v for k, v in actual.items()) or not 0 <= time.monotonic() - data['heartbeat'] <= 3:
            raise provider.AdmissionError()
        if pause:
            return {**data, 'status':'PAUSED', 'requires_owned_client':True, 'service_ready':False, 'sdk_ready':False}
        return data
    except Exception:
        return {'status':'PAUSED' if pause else 'UNKNOWN_NOT_READY', 'requires_owned_client':True, 'service_ready':False, 'sdk_ready':False}


def config_load(path):
    data = read(path).data
    allowed = set(SDK_KEYS) | {'manager_ws_url', 'manager_config_file', 'startup_timeout_s', 'call_timeout_s', 'shutdown_timeout_s', 'max_frame_bytes', 'config_id'}
    if not isinstance(data, dict) or set(data) - allowed:
        raise provider.AdmissionError()
    if data.get('testpilot_command') != '/home/vscode/.local/share/1c-testpilot/venv/bin/1c-testpilot' or data.get('testpilot_args', []) or data.get('descriptor_sha256') != '14b6d37d902dc381bc5e79b9d0b8c894990e68af244f19da62596fb6fbb318bd' or data.get('server_version') != '1.30.0' or data.get('manager_config_file') != '/home/vscode/.codex/config.toml':
        raise provider.AdmissionError()
    # Общая типизация без чтения фиктивного owner/binding.
    policy.validate_config({**data, 'binding_file':'/unused', 'action_policy_file':'/unused'})
    provider._child_env(data)
    return data


class Service:
    def __init__(self, config):
        self.config = config
        self.generation = uuid.uuid4().hex
        self.status = 'SDK_STARTING'
        self.task = None
        self.request = None
        self.accepted = False
        self.sid = None
        self.stop = asyncio.Event()
        self.who = identity()

    def publish(self):
        atomic('health.json', {**self.who, 'generation':self.generation, 'status':self.status,
            'requires_owned_client':self.status != 'ACTIVE', 'heartbeat':time.monotonic(),
            'stop_budget_s':provider.service_stop_budget(self.config)})

    def check_request(self):
        req = self.request.fresh()
        if req.get('service_generation') != self.generation or req.get('boot_id') != BOOT:
            raise provider.AdmissionError()
        if (ROOT / ('cancel-' + req['request_id'] + '.json')).exists():
            raise provider.AdmissionError()
        if not self.accepted and time.monotonic() >= req['deadline']:
            raise provider.AdmissionError()
        config = self.activation_config.fresh()
        self.binding.fresh()
        proof = self.proof.fresh()
        provider.verify_process(self.binding.data, proof)
        return config

    async def finish(self, sdk):
        if self.task:
            self.status = 'REVOKING'
            self.publish()
            if sdk.provider:
                sdk.provider._dispatch_ready = False
            self.task.cancel()
            try:
                await asyncio.wait_for(asyncio.shield(self.task), provider.relay_cleanup_budget(self.config))
            except asyncio.CancelledError:
                if not self.task.done():
                    raise
            except Exception:
                # qualified_relay сам помечает неизвестный effect/disconnect.
                # Завершённый clean rejection не загрязняет живой SDK.
                if not self.task.done():
                    sdk.dirty = True
                    raise RelayCleanupFailed() from None
            self.task = None
        sdk.admission_guard = None
        self.request = None
        self.accepted = False
        self.sid = None

    async def begin(self, sdk):
        with control():
            request = read(ROOT / 'request.json')
            req = request.data
            if not isinstance(req.get('request_id'),str) or len(req['request_id']) != 32:
                raise provider.AdmissionError()
            self.request = request
            self.activation_config = read(req['config_path'])
            if list(self.activation_config.identity) != req['config_identity']:
                raise provider.AdmissionError()
            config = policy.validate_config(self.activation_config.data)
            for key in SDK_KEYS + ('manager_ws_url','manager_config_file'):
                default = [] if key == 'testpilot_args' else None
                if config.get(key,default) != self.config.get(key,default):
                    raise provider.AdmissionError()
            self.binding = read(config['binding_file'])
            self.proof = read(config['owner_receipt_file'])
            policy.validate_binding(self.binding.data)
            if self.binding.data.get('connection_id'):
                raise provider.AdmissionError()
            self.check_request()
            sdk.admission_guard = self.check_request
            self.status = 'ACTIVATING'
            self.task = asyncio.create_task(provider.run_async(config, startup_receipt=lambda sid:setattr(self,'sid',sid), shared_sdk=sdk))

    async def tick(self, sdk):
        if self.task:
            try:
                self.check_request()
                if self.task.done():
                    await self.task
                    raise provider.TransportError()
                if not self.accepted and sdk.provider and isinstance(sdk.provider._session_id,str) and sdk.provider._session_id:
                    with control():
                        self.check_request()
                        atomic('ack.json', {'request_id':self.request.data['request_id'], 'service_generation':self.generation, 'status':'ACTIVE', 'session_id':sdk.provider._session_id})
                        self.check_request()
                        self.accepted = True
                        self.status = 'ACTIVE'
            except Exception:
                await self.finish(sdk)
                self.status = 'STANDBY'
        elif (ROOT / 'request.json').exists():
            try:
                current = read(ROOT / 'request.json')
                if current.identity != getattr(self, 'last_request', None):
                    self.last_request = current.identity
                    await self.begin(sdk)
            except Exception:
                self.request = None
                self.status = 'STANDBY'
        if sdk.invalid.is_set():
            self.status = 'DESCRIPTOR_REQUALIFYING'
            self.publish()
            await self.finish(sdk)
            try:
                await asyncio.wait_for(sdk.qualify(), self.config.get('startup_timeout_s',30))
            except provider.AdmissionError:
                self.status = 'DESCRIPTOR_INVALID'
                self.publish()
                await self.stop.wait()
            self.status = 'STANDBY'
        if sdk.dirty:
            raise provider.TransportError()

    async def run(self):
        delay = 1
        while not self.stop.is_set():
            self.status = 'SDK_STARTING'
            self.publish()
            exited = False
            try:
                async with provider.sdk_lifetime(self.config) as sdk:
                    self.status = 'STANDBY'
                    delay = 1
                    probe = None
                    try:
                        while not self.stop.is_set():
                            await self.tick(sdk)
                            if probe is None or probe.done():
                                if probe:
                                    await probe
                                probe = asyncio.create_task(asyncio.wait_for(sdk.session.send_ping(), 2))
                            self.publish()
                            await asyncio.sleep(0.2)
                    finally:
                        if probe:
                            probe.cancel()
                            await asyncio.gather(probe, return_exceptions=True)
                        await self.finish(sdk)
                exited = True
            except provider.SDKReapedError as exc:
                if isinstance(exc.cause, RelayCleanupFailed):
                    self.status = 'CLEANUP_FAILED'
                    self.publish()
                    await self.stop.wait()
                    return
                if isinstance(exc.cause, provider.AdmissionError):
                    self.status = 'DESCRIPTOR_INVALID'
                    self.publish()
                    await self.stop.wait()
                    return
                exited = True
            except provider.AdmissionError:
                self.status = 'DESCRIPTOR_INVALID'
                self.publish()
                await self.stop.wait()
                return
            except BaseException as exc:
                # stdio context должен успешно закончить cleanup до повторного spawn.
                # Ошибка context exit не доказывает reap: fail closed вместо retry.
                self.status = 'CLEANUP_FAILED'
                self.publish()
                if isinstance(exc, (KeyboardInterrupt, SystemExit, asyncio.CancelledError)):
                    raise
                await self.stop.wait()
                return
            if exited and not self.stop.is_set():
                self.status = 'SDK_FAILED'
                self.publish()
                try:
                    await asyncio.wait_for(self.stop.wait(),delay)
                except TimeoutError:
                    pass
                delay = min(delay * 2, 30)
        self.status = 'STOPPED'
        self.publish()


async def activate(path):
    path = str(Path(path).absolute())
    frozen = read(path)
    timeout = policy.validate_config(frozen.data).get('startup_timeout_s',30)
    reqid = uuid.uuid4().hex
    with control():
        info = health()
        if info['status'] != 'STANDBY':
            return {'status':'BUSY' if info['status'] in ('ACTIVE','ACTIVATING') else 'UNKNOWN_NOT_READY'}
        try:
            old = read(ROOT / 'request.json').data
            if old.get('service_generation') == info['generation'] and time.monotonic() < old.get('deadline',0) and not (ROOT / ('cancel-'+old['request_id']+'.json')).exists():
                return {'status':'BUSY'}
        except FileNotFoundError:
            pass
        deadline = time.monotonic() + timeout
        atomic('request.json', {'schema_version':1,'request_id':reqid,'service_generation':info['generation'],'boot_id':BOOT,'deadline':deadline,'config_path':str(Path(path).absolute()),'config_identity':list(frozen.identity)})
    success = False
    try:
        while time.monotonic() < deadline:
            try:
                ack = read(ROOT / 'ack.json').data
                if time.monotonic() < deadline and ack.get('request_id') == reqid and ack.get('service_generation') == info['generation'] and ack.get('status') == 'ACTIVE':
                    success = True
                    return ack
            except FileNotFoundError:
                pass
            await asyncio.sleep(0.1)
        return {'status':'ACTIVATION_EXPIRED'}
    finally:
        if not success:
            with control():
                atomic('cancel-'+reqid+'.json', {'request_id':reqid,'boot_id':BOOT})


def supervisor_state(command):
    with control():
        if command == 'pause':
            atomic('pause.json', {'boot_id':BOOT})
        elif command == 'resume':
            with provider.project_lock():
                with contextlib.suppress(FileNotFoundError):
                    (ROOT / 'pause.json').unlink()
        return {'status':'PAUSED' if paused() else 'RESUMED'}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('command', choices=['serve','activate','health','pause','resume','stop','supervise','pause-status'])
    parser.add_argument('--config')
    args = parser.parse_args()
    try:
        ensure_root()
        if args.command == 'pause-status':
            with control():
                result = {'status':'PAUSED' if paused() else 'RESUMED'}
                print(json.dumps(result),flush=True)
                return 3 if paused() else 0
        elif args.command == 'health':
            result = health()
        elif args.command in ('pause','resume'):
            result = supervisor_state(args.command)
        elif args.command == 'activate':
            result = asyncio.run(activate(args.config))
        elif args.command == 'stop':
            if not paused():
                raise provider.AdmissionError()
            data = health()
            if data['status'] == 'UNKNOWN_NOT_READY':
                raise provider.AdmissionError()
            pidfd = os.pidfd_open(data['pid'])
            try:
                if identity(data['pid']) != {k:data[k] for k in identity(data['pid'])}:
                    raise provider.AdmissionError()
                signal.pidfd_send_signal(pidfd, signal.SIGTERM)
            finally:
                os.close(pidfd)
            limit = time.monotonic() + data['stop_budget_s']
            while time.monotonic()<limit:
                try:
                    if identity(data['pid']) != {k:data[k] for k in identity(data['pid'])}:
                        break
                except FileNotFoundError:
                    break
                time.sleep(0.1)
            else:
                raise provider.TransportError()
            with provider.project_lock():
                pass
            result = {'status':'STOPPED'}
        elif args.command == 'supervise':
            # Этот процесс держит control lock через spawn, pause ACK исключает гонку.
            import subprocess
            with control():
                if paused():
                    print(json.dumps({'status':'PAUSED'}), flush=True)
                    return 0
                child = subprocess.Popen([sys.executable,__file__,'serve','--config',args.config])
            try:
                return child.wait()
            finally:
                if child.poll() is None:
                    child.terminate()
                    child.wait(timeout=provider.service_stop_budget(config_load(args.config)))
        else:
            with provider.project_lock():
                with control():
                    if paused():
                        print(json.dumps({'status':'PAUSED'}))
                        return 0
                try:
                    bootstrap = config_load(args.config)
                except (provider.AdmissionError, policy.ConfigError, json.JSONDecodeError) as exc:
                    raise PermanentConfigError() from None
                app = Service(bootstrap)
                async def run():
                    async def heartbeat():
                        while True:
                            app.publish()
                            await asyncio.sleep(0.5)
                    pulse = asyncio.create_task(heartbeat())
                    loop = asyncio.get_running_loop()
                    for sig in (signal.SIGTERM, signal.SIGINT):
                        loop.add_signal_handler(sig, app.stop.set)
                    try:
                        await app.run()
                    finally:
                        pulse.cancel()
                        await asyncio.gather(pulse, return_exceptions=True)
                asyncio.run(run())
            result = {'status':'STOPPED'}
        print(json.dumps(result),flush=True)
        return 0
    except provider.TransportError:
        print(json.dumps({'status':'BUSY'}),flush=True)
        return 2
    except PermanentConfigError:
        print(json.dumps({'status':'CONFIG_INVALID'}),flush=True)
        return 78
    except (provider.AdmissionError, policy.ConfigError, json.JSONDecodeError):
        print(json.dumps({'status':'REQUEST_REJECTED'}),flush=True)
        return 4
    except (OSError, ImportError, TimeoutError):
        print(json.dumps({'status':'OPERATIONAL_FAILED'}),flush=True)
        return 75
    except Exception:
        print(json.dumps({'status':'INTERNAL_FAILED'}),flush=True)
        return 70


if __name__ == '__main__':
    sys.exit(main())
#++agent TASK-225
