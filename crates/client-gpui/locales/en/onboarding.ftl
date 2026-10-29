# Onboarding strings (en). Referenced from src/onboarding/*.rs and checked by tests/onboarding.rs.

onboarding-title = Connect your first agent
onboarding-region-a11y = Get started
onboarding-intro = Pick a way to get an agent running. The quickest needs no keys and no cloud workspace: run one command on your computer.

onboarding-model-key-title = Add a model API key
onboarding-model-key-body = For cloud workspaces: the key is stored in the hub's secrets store.
onboarding-model-key-button = Add model key
onboarding-model-key-a11y = Add a model API key, not available yet
onboarding-model-key-disabled = Not available yet: the hub cannot store model keys until its secrets API ships (M2-002, phase 4). Use a computer below for now.

onboarding-byo-title = Add your own agent by URL
onboarding-byo-body = Point the hub at an A2A agent you already run.
onboarding-byo-button = Add A2A URL
onboarding-byo-a11y = Add a bring-your-own A2A URL, not available yet
onboarding-byo-disabled = Not available yet: the hub cannot register A2A agents until the A2A edge ships (M2-004, phase 5). Use a computer below for now.

onboarding-rc-title = Run sm rc on your computer
onboarding-rc-body = Needs no keys and no cloud workspace. Your computer runs the agent; this app controls it.
onboarding-rc-step-login = 1. Sign the computer in to this hub. It shows a code to approve in your browser; you do this once per computer.
onboarding-rc-step-rc = 2. Start remote control in the folder you want the agent to work in.
onboarding-rc-expect = You should see a QR code and a line starting with "session:". Press the space bar to show the QR code again. Leave the command running.
onboarding-rc-step-wait = 3. Come back here. This page moves on to your inbox by itself as soon as the computer appears, usually within a few seconds.
onboarding-copy = Copy
onboarding-copy-login-a11y = Copy the sign-in command
onboarding-copy-rc-a11y = Copy the sm rc command
onboarding-copied = Copied
onboarding-waiting = Waiting for a computer to connect...
onboarding-skip = Skip for now
onboarding-skip-a11y = Skip setup and go to the inbox
