# Machines screen strings (en) and the task view's "waiting for a machine" banner. Referenced from
# src/machines/*.rs and src/task/waiting.rs, checked by tests/machines.rs.

machines-title = Machines
machines-region-a11y = Machines
machines-intro = The computers that run your tasks. Run sm rc on a computer to add it.
machines-empty = No machines yet. Run sm rc on a computer to add one.
machines-table-a11y = Machines list
machines-col-name = Name
machines-col-kind = Kind
machines-col-presence = Presence
machines-col-agents = Agents
machines-presence-online = Online
machines-presence-asleep = Asleep
machines-presence-offline = Offline
machines-presence-unknown = Unknown
machines-presence-since = { $state } since { $since }
machines-kind-local = Computer
machines-kind-sandbox = Sandbox
machines-kind-unknown = Machine
machines-agents-none = None reported
machines-row-a11y = { $name }, { $kind }, { $presence }
machines-rename = Rename
machines-rename-a11y = Rename { $name }
machines-rename-placeholder = Machine name
machines-rename-field-a11y = New name for { $name }
machines-rename-save = Save
machines-rename-save-a11y = Save the new name for { $name }
machines-cancel = Cancel
machines-cancel-a11y = Cancel
machines-revoke = Revoke
machines-revoke-a11y = Revoke { $name }
machines-revoke-confirm = Revoke { $name }? It is disconnected at once, and tasks waiting for it are lost. It can only come back with a new credential.
machines-revoke-yes = Yes, revoke
machines-revoke-yes-a11y = Confirm: revoke { $name }
machines-name-empty = Enter a name.
machines-name-too-long = A name can be at most { $max } characters.
machines-status-working = Working...
machines-status-no-hub = Sign in to manage machines.
machines-status-renamed = Renamed to { $name }.
machines-status-revoked = Revoked { $name }.
machines-error-forbidden = Only admins can rename or revoke machines.
machines-error-not-found = That machine no longer exists.
machines-error-other = The hub refused: { $detail }
machines-error-network = Could not reach the hub: { $detail }

# Task view banner (src/task/waiting.rs).
machines-waiting = Waiting for { $machine }. It is offline; the queued messages start when it is back.
machines-waiting-a11y = Waiting for { $machine }, which is offline. { $count } queued.
machines-waiting-item-a11y = Queued for { $machine }: { $preview }
machines-waiting-item-empty = (empty message)
