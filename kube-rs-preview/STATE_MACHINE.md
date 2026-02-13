# OdooInstance State Machine

Auto-generated from the `TRANSITIONS` table in `controller/state_machine.rs`.

```mermaid
stateDiagram-v2
    [*] --> Provisioning

    Provisioning --> Uninitialized
    Provisioning --> Starting

    Uninitialized --> Initializing
    Uninitialized --> Restoring

    Initializing --> Starting : / MarkDbInitialized
    Initializing --> InitFailed

    InitFailed --> Initializing
    InitFailed --> Restoring

    Starting --> Stopped
    Starting --> Restoring
    Starting --> Upgrading
    Starting --> BackingUp
    Starting --> Running

    Running --> Stopped
    Running --> Restoring
    Running --> Upgrading
    Running --> BackingUp
    Running --> Degraded
    Running --> Starting

    Degraded --> Stopped
    Degraded --> Restoring
    Degraded --> Upgrading
    Degraded --> BackingUp
    Degraded --> Running
    Degraded --> Starting

    BackingUp --> Stopped
    BackingUp --> Running
    BackingUp --> Degraded
    BackingUp --> Starting

    Upgrading --> Starting

    Restoring --> Starting : / MarkDbInitialized
    Restoring --> Starting

    Stopped --> Restoring
    Stopped --> Upgrading
    Stopped --> Starting

    Error --> Starting
    Error --> Uninitialized
```
