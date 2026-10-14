# corvo-lan

Serverless chat for your local network. Run it on two machines on the same
Wi-Fi or LAN and they find each other. There's no server, no account and no
setup.

- People find each other automatically (UDP multicast), then talk over direct TCP connections.
- Everyone gets a unique name: you choose the start and a word is added (`--nick bob` → `@bobRoger`).
- Private rooms are protected by a key and end-to-end encrypted. Only people with the room's name and key can read it.
- Direct messages go peer to peer, and every connection is encrypted.
- Chats are never written to disk. Quitting forgets everything.

## Install

On macOS or Linux (x86_64 or ARM):

```bash
curl -fsSL https://raw.githubusercontent.com/Abinsanth/corvo-lan/release/install.sh | sh
```

This downloads the latest release, checks its SHA-256, and installs it to
`~/.local/bin` (set `CORVO_LAN_INSTALL_DIR` to change that). It never uses sudo.
Run the same command again to update.

On Windows 10 or 11 (x86_64; **experimental**), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/Abinsanth/corvo-lan/release/install.ps1 | iex
```

This installs to `%LOCALAPPDATA%\Programs\corvo-lan` (set `CORVO_LAN_INSTALL_DIR`
to change that) and adds it to your PATH; open a new terminal afterwards. No
admin rights needed. Windows support is new and hasn't had much testing yet;
please open an issue if something doesn't work.

Or build from source with a Rust toolchain ([rustup.rs](https://rustup.rs)):

```bash
git clone https://github.com/Abinsanth/corvo-lan.git
cd corvo-lan
cargo install --path .
```

### Check the install

Open a new terminal and run:

```bash
corvo-lan --version         # prints e.g. "corvo-lan 0.4.0"
command -v corvo-lan        # macOS/Linux: where it's installed
```

```powershell
Get-Command corvo-lan       # Windows: where it's installed
```

If the shell says the command isn't found, the install folder isn't on your
PATH yet (see the hint the installer printed) or the terminal was opened
before installing.

### Uninstall

On macOS or Linux, delete the binary, then, if you also want to forget your
identity key and the peers you've met, the config folder:

```bash
rm ~/.local/bin/corvo-lan   # or $CORVO_LAN_INSTALL_DIR/corvo-lan
rm -rf ~/.config/corvo-lan  # or $XDG_CONFIG_HOME/corvo-lan
```

If you built it with `cargo install`, use `cargo uninstall corvo-lan` instead.

On Windows, in PowerShell:

```powershell
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\corvo-lan"
Remove-Item -Recurse -Force "$env:APPDATA\corvo-lan"   # identity key and known peers
```

Then remove `%LOCALAPPDATA%\Programs\corvo-lan` from your PATH: search the Start
menu for "Edit environment variables for your account", select `Path`, and
delete that entry.

Deleting the config folder gives you a new identity next time, so peers you've
met will see a key-change warning.

## Use

```bash
corvo-lan                   # your name starts with your login name
corvo-lan --nick bob        # or choose the start yourself
```

After about two seconds you get a name, and you're connected to everyone else running it.

```
/create cat 123            # creates e.g. #catPieR with key 123-k7m9xq
/chat #catPieR 123-k7m9xq  # someone else joins with the name and key
/chat @bobRoger            # private chat
/msg @bobRoger hi          # one-off private message
/chat bob                  # the @ or # is optional when only one name matches
/who                       # who's connected
/update [yes]              # check for and install a newer version
/close [@nick]             # close a private chat (TUI)
/line  /tui                # switch between the full-screen view and a plain prompt
/his [n]                   # recent messages here (line mode)
/help  /h                  # all commands
/quit  /q                  # leave
```

corvo-lan opens a full-screen view: tabs for each chat along the top, the
people in the room and everyone online on the right. `Shift-Tab`, `Alt-←`/`Alt-→` or
`Alt-1..9` switch tabs, `Tab` completes `@names`, `#rooms` and `/commands` (in
line mode too), `Alt+Enter` (or `Shift+Enter` where your terminal
supports it) adds a new line, `PgUp`/`PgDn` or the mouse wheel scroll, and
clicking a name opens a private chat. Command output and errors appear in the
`corvo` tab. To select text (say, a room key), hold Shift (Option on macOS)
while dragging, or switch to `/line`.

Prefer a plain prompt? Start with `--line`, or type `/line` (and `/tui` to come
back). Line mode is also used automatically when output isn't a terminal.

### Encryption

Everything between machines is encrypted, and each machine has its own key,
kept in `~/.config/corvo-lan/` (`%APPDATA%\corvo-lan` on Windows). `/who`
shows a short fingerprint for everyone (and you); compare them in person if you
want to be sure who you're talking to. If someone you've met before shows up
with a different key, corvo-lan warns you.

### Updating

Type `/update` to see newer versions, then `/update yes` to install the
newest one; restart corvo-lan to use it. On Windows the old version is set
aside as `corvo-lan.old.exe` and deleted the next time corvo-lan starts; `/update` there needs Windows 10
version 1803 or newer, which has the built-in curl and tar. At
startup corvo-lan makes one request to GitHub to see whether a newer version
exists. That request sends only your IP address and the app version; turn it
off with `--no-update-check` or `CORVO_LAN_NO_UPDATE_CHECK=1`. Nothing else
ever leaves your network.

### If peers don't find each other

Discovery uses multicast on `239.255.42.99:4299`. Some networks (guest Wi-Fi,
corporate networks, some routers) block it. In that case, connect directly:

```bash
corvo-lan --port 5000                      # on one machine
corvo-lan --peer 192.168.1.20:5000         # on the other
```

Your firewall must also allow incoming TCP on the port you use. On Windows,
allow corvo-lan on **private networks** when the firewall asks on first run,
and make sure your Wi-Fi is set to *Private* (Settings → Network & internet →
Wi-Fi → your network). `--debug` writes a log to `./corvo-lan.log`.

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

CI runs all three on every pull request.

## Releasing

Work happens on `main`. To ship, bump `version` in `Cargo.toml`, then merge
`main` into the `release` branch. That builds macOS, Linux and Windows
binaries, tags `v<version>`, and publishes them as a GitHub Release, which is
what the install script downloads. A version that's already released is
refused, so bump it first.

## License

MIT. See [LICENSE](LICENSE).
