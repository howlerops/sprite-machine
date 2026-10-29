# Agents screen strings (en). Referenced from src/agents/*.rs and checked by tests/agents.rs.

agents-title = Agents
agents-region-a11y = Agents
agents-intro = Agents available through your machines. A hub-side agent registry does not exist yet, so only local harnesses are listed.
agents-empty = No agents yet. A machine lists its harnesses after it connects to the hub.

agents-col-name = Agent
agents-col-kind = Kind
agents-col-runs-on = Runs on
agents-col-skills = Skills
agents-col-health = Health
agents-col-presence = Presence

agents-kind-local = Local harness
agents-kind-managed = Managed
agents-kind-byo = Bring your own
agents-presence-online = Online
agents-presence-offline = Offline
agents-presence-unknown = Unknown
agents-health-ready = Ready
agents-health-unreachable = Unreachable
agents-health-unknown = Unknown
agents-skills-none = Not reported
agents-row-a11y = { $name }, { $kind }, runs on { $machine }, { $presence }, health { $health }

agents-byo-button = Add BYO agent by URL
agents-byo-disabled = Bring-your-own agents need the A2A edge, which is not available yet.

agents-launch-title = Launch managed agent
agents-launch-button = Launch
agents-launch-disabled-no-key = Add a model API key in Settings before launching a managed agent.
agents-field-name = Name
agents-field-harness = Harness (claude, codex, opencode, pi, gemini, goose)
agents-field-model = Model
agents-field-egress = Egress allowlist (hosts, comma separated)
agents-field-loadout = Loadout (optional)
agents-error-name-required = Enter a name.
agents-error-harness-unknown = Unknown harness: { $value }
agents-error-model-required = Enter a model.
agents-error-egress-invalid = Not a valid host name: { $value }
agents-launch-ok = Launched agent { $id }.
agents-launch-failed = Could not launch the agent: { $reason }
agents-empty-setup = Connect a machine
agents-empty-setup-a11y = Show how to connect a machine
