# Reliability

ATX cannot make process creation atomic with whatever side effect a command
causes. It does not claim exactly-once execution.

Once a run enters `Starting`, the default is at-most-once:

1. Record `Starting`.
2. Try to create the command.
3. Record the known result.

A crash between those steps can leave the result unknown. ATX records that run
as `Interrupted` and does not retry it automatically.

Session mode survives terminal closure but not necessarily logout or reboot.
Durable mode asks the user's service manager to restart the supervisor. It
never silently falls back to session mode.

One-shot jobs missed while no supervisor is available are held by default.
Recurring jobs skip old occurrences and continue at the next anchored time.
