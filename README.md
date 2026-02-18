# rdp-screenshot

A simple tool for taking screenshots of remote Windows desktops using IronRDP.

Taken mostly as-is from the [IronRDP screenshot example](https://github.com/Devolutions/IronRDP/blob/d0874d6f84c4b719a796bca7dd44b6e9440073db/crates/ironrdp/examples/screenshot.rs).

Not affiliated with the IronRDP project.

## Usage

```bash
ironrdp-screenshot-tool --host <HOSTNAME> --port <PORT>
                        -u/--username <USERNAME> -p/--password <PASSWORD>
                        [-d/--domain <DOMAIN>] [-o/--output <OUTPUT_FILE.png>]
```