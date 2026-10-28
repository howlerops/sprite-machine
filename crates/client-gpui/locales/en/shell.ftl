# Desktop shell strings (en). One message per user-visible string; ids are referenced from
# src/shell/*.rs and checked by tests/shell.rs.

app-name = Sprite Machine

## Sign-in
login-title = Sign in to Sprite Machine
login-subtitle = Pair this computer with your phone, or continue with GitHub.
login-pair = Pair with a phone (QR code)
login-pair-a11y = Sign in by pairing: show a QR code to scan with your phone
login-github = Continue with GitHub
login-github-a11y = Sign in with GitHub in your system browser
login-cancel = Cancel
login-cancel-a11y = Cancel signing in
login-retry = Try again
login-retry-a11y = Go back and try signing in again
login-starting = Contacting the hub...
login-pairing-code = Code: { $code }
login-pairing-code-a11y = Pairing code { $code }
login-pairing-instructions = Scan the QR code with your phone, or open { $uri } and enter the code.
login-pairing-expires = The code expires in { $minutes } minutes.
login-qr-a11y = QR code for pairing. Scan it with your phone, or enter the pairing code instead.
login-browser-waiting = Finish signing in with GitHub in your browser. This window continues on its own.
login-signed-in = Signed in as { $user }
login-error-expired = The code expired before it was approved.
login-error-denied = Sign-in was declined.
login-error-network = Could not reach the hub: { $detail }
login-error-browser = Could not open your browser: { $detail }
login-error-timeout = Sign-in took too long and was stopped.
login-error-hub = The hub refused the sign-in: { $detail }
login-error-cancelled = Sign-in was cancelled.
login-status-a11y = Sign-in status

## Navigation
nav-label = Main navigation
nav-inbox = Inbox
nav-new-task = New task
nav-agents = Agents
nav-machines = Machines
nav-workspace = Workspace
nav-flows = Flows
nav-loadouts = Loadouts
nav-usage = Usage
nav-settings = Settings
nav-item-a11y = Go to { $section }
nav-current-a11y = { $section }, current page

## Content
content-a11y = Page content
content-placeholder = { $section } is not available in this build yet.
banner-offline = You are offline. Changes will be sent when the connection returns.
banner-connecting = Reconnecting...

## Settings
settings-title = Settings
settings-appearance = Appearance
settings-theme-system = Match system
settings-theme-light = Light
settings-theme-dark = Dark
settings-theme-a11y = Theme: { $choice }
settings-theme-selected-a11y = Theme: { $choice }, selected
settings-language = Language
settings-language-en = English
settings-hub = Hub address
settings-account = Account
settings-signed-in-as = Signed in as { $user } in { $org }
settings-sign-out = Sign out
settings-sign-out-a11y = Sign out of Sprite Machine on this computer
settings-not-signed-in = Not signed in
