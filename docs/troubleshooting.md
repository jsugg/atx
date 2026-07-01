# Troubleshooting

`atx doctor` will be the first stop once the command is implemented. It checks
directory ownership, SQLite settings, clocks, process identity support, the
supervisor socket, timezone data, and the available service manager.

For now, keep the failing command output and the isolated state directory used
for the run. Do not delete a suspect database: recovery should preserve it for
inspection.
