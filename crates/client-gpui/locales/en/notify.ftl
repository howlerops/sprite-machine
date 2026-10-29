# Notification strings (en). Referenced from src/notify/*.rs and checked by tests/notify.rs.
# Titles are generic on purpose: no machine names, prompts or agent output (plan §4.2).

notify-title-needs_you = Needs your attention
notify-title-done = Task finished
notify-title-failed = Task failed

notify-settings-title = Notifications
notify-settings-region-a11y = Notification settings
notify-settings-intro = Choose which thread changes send you a desktop notification.
notify-kind-needs_you = Needs you
notify-kind-needs_you-desc = A thread is waiting for an approval or an answer.
notify-kind-done = Done
notify-kind-done-desc = A thread finished its work.
notify-kind-failed = Failed
notify-kind-failed-desc = A thread stopped with an error.
notify-kind-on-a11y = { $name } notifications, on
notify-kind-off-a11y = { $name } notifications, off
notify-save-failed = Could not save notification settings: { $reason }
