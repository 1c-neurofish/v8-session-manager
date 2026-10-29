"""Функциональный offline проход готового кода; без сети и business fixtures."""
import asyncio
import argparse
import copy
import json
from pathlib import Path
import sys
import time

#++agent TASK-225 [29.10.2026 20:44:00]
# __file__ сохраняет standalone запуск вне project CWD.
adapter_dir = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(adapter_dir))
sys.path.insert(0, str(adapter_dir / 'tests'))
options = argparse.ArgumentParser()
options.add_argument('--sdk-inventory', type=Path, help='optional public actual SDK tools/list JSON')
arguments = options.parse_args()
#++agent TASK-225
import debugger as d
import provider as c
from _harness import SDKMemoryPeer, FakeWS

checks = []
def check(name, condition):
    assert condition, name
    checks.append(name)
def denied(name, function):
    try:
        function()
    except c.AdapterError:
        checks.append(name)
    else:
        raise AssertionError(name)

canary = 'OFFLINE_CANARY'
check('nested_json_decode', d.normalize(json.dumps({'value': json.dumps({'value': canary})}))['value']['value'] == canary)
check('unicode_escape_decode', d.normalize('"\\u004fFFLINE_CANARY"') == canary)
xml = d.normalize('<x password="outside_dictionary">OFFLINE&#95;CANARY</x>')
check('xml_entity_and_secret_key', xml['text'] == canary and xml['attributes']['password'] == 'outside_dictionary')
check('actual_json_keys_preserved', d.normalize({'password': 'outside_dictionary'}) == {'password': 'outside_dictionary'})
check('variable_name_not_key', d.normalize({'name': 'password', 'value': 'ordinary'}) == {'name': 'password', 'value': 'ordinary'})
check('unknown_source_preserved', d.normalize('ordinary') == 'ordinary')
denied('dtd_rejected', lambda: d.normalize('<!DOCTYPE x [<!ENTITY a "x">]><x>&a;</x>'))
denied('oversized_string', lambda: d.normalize('x' * 262145))
denied('depth_bound', lambda: d.normalize([[[[0]]]], depth=23))
denied('unsupported_binary', lambda: d.payload({'content':[{'type':'image','data':canary}]}))
encoded = d.envelope(d.payload({'content':[{'type':'text','text':json.dumps({'details':canary})}], 'structuredContent':{'value':canary}, 'isError':True}))
e = json.loads(encoded['content'][0]['text'])
check('envelope_manager_wire', set(encoded) == {'content','is_error'} and e['schema_version'] == 1 and e['field_sources'] == {} and encoded['is_error'])
check('all_error_representations', e['result']['content'][0]['details'] == canary and e['result']['structuredContent']['value'] == canary)
check('transport_error_safe', canary not in json.dumps(c.error_for(RuntimeError(canary))))

binding_shape = {'schema_version':1,'generation':'offline-generation','expires_at':time.time()+60,
 'native_session_id':'offline-selector','infobase_name':'gbig_pam_ai','cluster_server':'onec-infra',
 'ib_session_number':123,'endpoint':'http://onec-infra:1550','alias':'offline-alias',
 'endpoint_dev_verified':True,'allowed_target_ids':['own']}
check('dev_binding_shape', d.validate_binding(binding_shape) == binding_shape)
for label,delta in [('expired',{'expires_at':time.time()-1}),('prod',{'infobase_name':'gbig_pam'}),
                    ('unverified_mapping',{'endpoint_dev_verified':False}),('native_bool',{'ib_session_number':True}),
                    ('target_alias',{'allowed_target_ids':[{}]})]:
    denied('binding_'+label, lambda delta=delta: d.validate_binding({**binding_shape,**delta}))
from unittest.mock import patch
async def noop(*args,**kwargs): pass
with patch.object(c.policy,'load_json_file',return_value={}), patch.object(c.policy,'validate_config',return_value={}) as validate, patch.object(c,'run_async',noop):
    check('cli_default_preserved', c.main(['--config','offline']) == 0 and validate.call_count == 1)
with patch.object(c.policy,'load_json_file',return_value={}), patch.object(d,'run',noop) as run, patch.object(c.policy,'validate_config') as validate:
    check('cli_debugger_selected', c.main(['--profile','debugger','--config','offline']) == 0 and validate.call_count == 0)

async def relay_checks():
    descriptors = []
    for name in sorted(d.ALL_TOOLS):
        props = {}
        required = []
        if name in ('continue','step_in','step_out','pause','get_variables','evaluate','set_breakpoints','get_call_stack'):
            props['targetId'] = {'type':'string'}
            required.append('targetId')
        if name == 'evaluate':
            props['expression'] = {'type':'string'}
            required.append('expression')
        if name == 'get_performance_measurement':
            props['targetId'] = {'type':'string'}
        if name == 'raw_request':
            props.update(cmd={'type':'string'},xml={'type':'string'},dbgui={'type':'string'})
            required = ['cmd','xml']
        if name == 'reload_metadata':
            props['skipCache'] = {'type':'boolean'}
        if name == 'wait_for_stop':
            props['timeout'] = {'type':'number'}
        descriptors.append({'name':name, 'description':'offline', 'inputSchema':{'type':'object','properties':props,'required':required}})
    binding = {'infobase_name':'gbig_pam_ai','cluster_server':'onec-infra','ib_session_number':123,
               'allowed_target_ids':['own'], 'endpoint':'http://onec-infra:1550','alias':'private-alias'}
    async with SDKMemoryPeer(tools=descriptors) as peer:
        await peer.client_session.initialize()
        seen = []
        stop = ['own']
        async def answer(params):
            seen.append(params)
            name = params['name']
            if name == 'attach': data = {'sessionId':'PRIVATE_SESSION','url':binding['endpoint'],'infobaseAlias':binding['alias']}
            elif name == 'get_targets': data = {'targets':[{'targetID':{'id':'own'},'suspended':False},{'targetID':{'id':'foreign'},'suspended':False}]}
            elif name == 'wait_for_stop': data = {'targetId':stop[0], 'moduleName':canary}
            elif name == 'raw_request': data = {'status':200, 'body':'<result>OFFLINE&#95;CANARY</result>'}
            elif name == 'get_performance_measurement': data = {'measurements':[{'moduleData':[{'moduleName':canary}]}]}
            elif name == 'get_variables': data = {'variables':[{'name':'x','typeName':'String','value':json.dumps({'value':canary})}]}
            else: data = {'success':True}
            return {'content':[{'type':'text','text':json.dumps(data)}]}
        peer.call_handler = answer
        ws = FakeWS()
        async def admission(): return None
        config = {'descriptor_sha256':c.descriptor_digest(descriptors),'call_timeout_s':0.5,'shutdown_timeout_s':0.5}
        p = d.DebuggerProvider(config,binding,mcp_session=peer.client_session,relay_port=peer.relay_port,ws=ws,admission_check=admission)
        p._server_version = '1.0.0'
        await p._fetch_tools()
        check('public_inventory', {x['name'] for x in p._tools} == set(d.PUBLIC))
        registration = asyncio.create_task(p.register())
        frame = await ws.manager_recv_json()
        check('registration_kind_and_native', frame['params']['kind'] == 'debugger' and frame['params']['ib_session_number'] == 123)
        await ws.manager_reply(frame['id'], result={'session_id':'offline-public'})
        await registration
        async def call(name,args={}):
            await p._dispatch_call(str(len(seen)), {'name':name,'arguments':args})
            return await ws.manager_recv_json()
        for name,args in [('debug_attach',{'url':'foreign'}),('raw_request',{}),('debug_missing_method',{})]:
            r = await call(name,args)
            check('deny_'+name+str(len(checks)), 'error' in r and not seen)
        r = await call('debug_attach', {'session_id':'strip','database_key':'strip','_v8_call_options':{}})
        check('attach_output_private', 'PRIVATE_SESSION' not in json.dumps(r) and 'private-alias' not in json.dumps(r) and 'status' in json.loads(r['result']['content'][0]['text'])['result'])
        check('private_attach_pinned', seen[-1]['arguments'] == {'url':binding['endpoint'],'infobaseAlias':binding['alias'],'autoAttach':False})
        check('private_ui_captured', p.debug_session_id == 'PRIVATE_SESSION')
        for action, xml in [('on','<request xmlns="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse"><infoBaseAlias>foreign</infoBaseAlias><idOfDebuggerUI>foreign</idOfDebuggerUI><measureModeSeanceID>mode-key</measureModeSeanceID></request>'),('off','<request xmlns="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse"></request>')]:
            r = await call('debug_raw_request',{'cmd':'setMeasureMode','xml':xml})
            check('raw_measure_'+action, 'result' in r and seen[-1]['name']=='raw_request' and seen[-1]['arguments']['cmd']=='setMeasureMode')
            check('raw_identity_'+action, 'foreign' not in seen[-1]['arguments']['xml'] and 'private-alias' in seen[-1]['arguments']['xml'] and 'PRIVATE_SESSION' in seen[-1]['arguments']['xml'] and seen[-1]['arguments']['dbgui']=='PRIVATE_SESSION')
            check('measure_state_'+action, p.measurement_active == (action=='on'))
            check('raw_xml_result_'+action, json.loads(r['result']['content'][0]['text'])['result']['content'][0]['body']['text']==canary)
        r = await call('debug_get_performance_measurement')
        check('performance_optional_target_without_stop','result' in r and json.loads(r['result']['content'][0]['text'])['result']['content'][0]['measurements'][0]['moduleData'][0]['moduleName']==canary)
        r = await call('debug_reload_metadata')
        check('metadata_reload_mapped','result' in r and seen[-1]['name']=='reload_metadata')
        r = await call('debug_get_targets')
        check('own_base_discovery' , 'foreign' in json.dumps(r) and 'own' in json.dumps(r))
        p._binding['allowed_target_ids'] = []
        r = await call('debug_get_targets')
        check('discovery_empty_allowlist_returns_own_base_list', 'foreign' in json.dumps(r) and 'own' in json.dumps(r) and not p.current_targets)
        before = len(seen)
        r = await call('debug_pause', {'targetId':'own'})
        check('empty_allowlist_control_denied', 'error' in r and all(x['name'] == 'get_targets' for x in seen[before:]))
        p._binding['allowed_target_ids'] = ['own']
        before = len(seen)
        r = await call('debug_pause',{'targetId':'foreign'})
        check('foreign_target_not_commanded', 'error' in r and all(x['name'] == 'get_targets' for x in seen[before:]))
        r = await call('debug_get_variables',{'targetId':'own'})
        check('inspection_requires_stop','error' in r)
        r = await call('debug_wait_for_stop')
        check('own_stop', 'result' in r)
        r = await call('debug_get_variables',{'targetId':'own'})
        check('sdk_relay_normalized', json.loads(r['result']['content'][0]['text'])['result']['content'][0]['variables'][0]['value']['value'] == canary)
        stop[0] = 'foreign'
        r = await call('debug_wait_for_stop')
        check('foreign_stop_denied','error' in r and 'foreign' not in json.dumps(r))
        async def slow(params):
            if params['name'] == 'get_targets':
                return await answer(params)
            await asyncio.sleep(2)
            return {'content':[]}
        p.stopped = {'own'}
        peer.call_handler = slow
        r = await call('debug_evaluate',{'targetId':'own','expression':'ordinary'})
        # Refresh is bounded too; cancellation must close admission after uncertain outcome.
        check('timeout_safe','error' in r and not p._dispatch_ready)
        await asyncio.sleep(0.01)
        check('no_pending_leak',not p._pending and not p._capture_queue)
        check('sdk_cancel_actual_id',bool(peer.cancelled_notifications) and peer.cancelled_notifications[-1]['requestId'] == peer.seen_call_ids[-1])
        peer.call_handler = answer
        p.breakpoints = True
        before = len(seen)
        await p.cleanup_debugger()
        check('own_cleanup', [x['name'] for x in seen[before:]] == ['clear_breakpoints','detach'] and not p.attached)
        for failing_stage in ('admission', 'clear_breakpoints', 'refresh', 'continue'):
            p.attach_attempted = p.attached = True
            p._cleanup_failed = False
            p.restore_running = {'own'}
            p.breakpoints = failing_stage == 'clear_breakpoints'
            cleanup_calls = []
            original_error = c.AdmissionError()
            async def fail_admission():
                if failing_stage == 'admission': raise original_error
            async def cleanup_invoke(name,args):
                from types import SimpleNamespace
                cleanup_calls.append(name)
                if name == failing_stage: raise original_error
                return SimpleNamespace(isError=False)
            async def refresh():
                if failing_stage == 'refresh': raise original_error
                p.current_targets = {'own'}
                p.target_states = {'own':True}
            original_invoke, original_refresh, original_admission = p.invoke_private, p.refresh_targets, p._admission_check
            p.invoke_private, p.refresh_targets, p._admission_check = cleanup_invoke, refresh, fail_admission
            try:
                try: await p.cleanup_debugger()
                except c.AdmissionError as exc:
                    check('cleanup_original_error_'+failing_stage, exc is original_error)
                else: raise AssertionError('cleanup_error_missing_'+failing_stage)
                check('detach_after_'+failing_stage, cleanup_calls[-1] == 'detach' and not p.attached and p._cleanup_failed)
                if failing_stage == 'admission':
                    check('no_resume_admission_bypass', cleanup_calls == ['detach'])
            finally:
                p.invoke_private, p.refresh_targets, p._admission_check = original_invoke, original_refresh, original_admission
        p.attach_attempted = p.attached = True
        p.measurement_active = True
        p.breakpoints = False
        p.restore_running = set()
        cleanup_calls = []
        async def measurement_cleanup(name,args):
            from mcp.types import CallToolResult,TextContent
            cleanup_calls.append((name,args))
            data = {'status':200,'body':''} if name=='raw_request' else {'success':True}
            return CallToolResult(content=[TextContent(type='text',text=json.dumps(data))])
        original = p.invoke_private
        p.invoke_private = measurement_cleanup
        try:
            await p.cleanup_debugger()
            check('measurement_finally_off_before_detach',[x[0] for x in cleanup_calls]==['raw_request','detach'] and cleanup_calls[0][1]['cmd']=='setMeasureMode' and 'measureModeSeanceID' not in cleanup_calls[0][1]['xml'] and not p.measurement_active)
        finally:p.invoke_private = original
        check('real_sdk_ids_observed',len(peer.seen_call_ids) >= 6)
        p._config['descriptor_sha256'] = '0'*64
        p._tools = None
        try: await p._fetch_tools()
        except c.AdmissionError: checks.append('schema_drift_denied')
        else: raise AssertionError('schema_drift_denied')
        await ws.close()
        await p._loop_task

#++agent TASK-225 [29.10.2026 20:44:00]
# Actual snapshot явно передаётся; default offline использует bundled public policy.
actual_inventory = json.loads(arguments.sdk_inventory.read_text()) if arguments.sdk_inventory else [{'name':name} for name in json.loads((adapter_dir / 'debugger-policy.json').read_text())['upstream_tools']]
#++agent TASK-225
if isinstance(actual_inventory,dict): actual_inventory = actual_inventory.get('tools',actual_inventory.get('descriptors'))
check('full_actual_sdk_names_mapped_16', {x['name'] for x in actual_inventory} == set(d.TOOLS) and len(d.PUBLIC)==16)
asyncio.run(asyncio.wait_for(relay_checks(),10))
print(json.dumps({'status':'passed','count':len(checks),'checks':checks,'dictionary_live_tested':False,'real_sdk_memory_transport':True,'actual_inventory_supplied':arguments.sdk_inventory is not None},indent=2))
