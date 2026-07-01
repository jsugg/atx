# CLI Notes

The main form is:

```text
atx <when> [options] -- <program> [argument...]
```

The first literal `--` separates ATX arguments from the command. Everything
after it stays as an argv array. Shell mode is opt-in and accepts one string.

The full command and exit-code reference will be generated and checked once the
parser is complete.
