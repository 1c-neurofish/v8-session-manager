## 6. Представление времени выполнения

### 6.1 Регистрация клиента

```mermaid
sequenceDiagram
    participant Client as 1С-клиент<br/>(addin + devkit BSL)
    participant WS as transport.rs (WS :4000)
    participant Reg as SessionRegistry
    participant Disp as SessionDispatcher
    participant Notify as notify.rs

    Client->>WS: WS handshake /sessions
    WS->>Reg: на коннект — пустая запись Reserved
    Client->>WS: session.register {client_uid, prefix, host_id, pid, tools}
    WS->>Reg: register_or_reattach(client_uid, generation, prefix, tools)
    alt Новый client_uid
        Reg-->>WS: created (Active)
        Reg->>Disp: spawn dispatcher
    else Тот же client_uid в reconnection_grace
        Reg-->>WS: reattached (Active, generation+=1)
    end
    WS-->>Client: session.register.result
    Reg->>Notify: tools changed
    Notify-->>"MCP HTTP-клиенты": tools/list_changed
```

### 6.2 Tool-вызов

```mermaid
sequenceDiagram
    participant Agent as AI-агент
    participant Http as MCP HTTP (:4001)
    participant Reg as SessionRegistry
    participant Disp as SessionDispatcher
    participant WS as WS transport
    participant Client as 1С-клиент

    Agent->>Http: tools/call <prefix>__<tool>
    Http->>Reg: lookup prefix → session_id
    Http->>Disp: enqueue(call)
    Disp->>WS: tools/call (когда сессия свободна)
    WS->>Client: WS frame
    Client-->>WS: tools/result
    WS-->>Disp: deliver result + bump last_call_at
    Disp-->>Http: result
    Http-->>Agent: MCP response
```

Ключевые свойства:

- очередь — на сессию, а не глобальная: разные сессии исполняют параллельно;
- порядок tool-вызовов в одной сессии сохраняется (ADR-0021);
- ошибки транспорта (WS rotting / disconnect в момент вызова) маршрутизируются в MCP-ошибку с пометкой о soft-reconnect окне.

### 6.3 Liveness и soft-reconnect

```mermaid
sequenceDiagram
    participant Mgr as transport.rs (writer)
    participant Client as addin (tokio worker)
    participant Reg as SessionRegistry
    participant LC as lifecycle.rs

    loop каждые ws_ping_interval_ms
        Mgr->>Client: WS Ping (RFC 6455)
        Client-->>Mgr: WS Pong (автоматически, без BSL)
        Mgr->>Reg: bump last_inbound_at
    end
    Note over Mgr,Client: Pong не пришёл за ws_ping_timeout_ms
    Mgr->>Reg: mark_disconnected_if_generation
    Reg->>LC: запись Disconnected, grace=reconnection_grace_secs
    alt Клиент успел переподключиться
        Client->>Mgr: новый WS + session.register (тот же client_uid)
        Mgr->>Reg: register_or_reattach → Active, generation+=1
    else Grace истёк
        LC->>Reg: remove_if_generation
    end
```

### 6.4 Закрытие

- Корректное: клиент шлёт `session.bye` → `Reg::remove_if_generation`. Diff попадает в `tools/list_changed`.
- Idle: `lifecycle.rs::idle_sweeper` периодически удаляет записи с `last_call_at + idle_timeout_secs < now`. После ADR-0034 фильтр по `origin` не применяется.
- Graceful shutdown: SIGTERM → `app.rs` ставит CancellationToken, оба транспорта дренируют inflight-вызовы в пределах `mcp.execution.shutdown_grace_period_secs`.
