# corvo-lan

Serverless chat for your local network. Run it on two machines on the same
Wi-Fi or LAN and they find each other. There's no server, no account and no
setup.

- People find each other automatically (UDP multicast), then talk over direct TCP connections.
- Everyone gets a unique name: you choose the start and a word is added (`--nick Bob` → `@bobroger`). Names are all lowercase.
- Private rooms are end-to-end encrypted, and the people inside decide who joins. A room's key only lets you ask.
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
corvo-lan --version         # prints "corvo-lan <version>"
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

It shows the name you'd get: press Enter to keep it, or type another start to see a new one.
Then you're connected to everyone else running it. With `--nick` it skips the question.

```
/create cat 5              # a room for up to 5 people, e.g. #catpie with key k7m9xq
/invite @bobroger          # invite someone; they /accept (your invite counts as your approval)
/chat #catpie k7m9xq       # or ask to join with a room's key (click it in the sidebar to copy)
/approve @bobroger         # someone in the room lets them in (or /deny @bobroger)
/kick @bobroger            # vote someone out of the room
/chat @bobroger            # private chat
/msg @bobroger hi          # one-off private message
/chat bob                  # the @ or # is optional when only one name matches
/who                       # who's connected
/update [yes]              # check for and install a newer version
/close [@nick]             # close a private chat (TUI)
/help  /h                  # show commands (q or Esc closes it)
/quit  /q                  # leave
```

corvo-lan opens a full-screen view: tabs for each chat along the top, the
people in the room and everyone online on the right. `Shift-Tab`,
`Alt-←`/`Alt-→` or `Alt-1..9` switch tabs, `Tab` completes `@names`, `#rooms`
and `/commands`, `Alt+Enter` (or `Shift+Enter` where your terminal supports it)
adds a new line, `PgUp`/`PgDn` or the mouse wheel scroll, and clicking a name
opens a private chat. Command output and errors appear in the `corvo` tab. The
bottom line shows your `ip` and `port`: on most home and office networks,
people on the same LAN share the first three numbers of the ip. To select
text, hold Shift (Option on macOS) while dragging.

In a private chat, each line has a mark after the time: `←` for messages you
received, and for yours `·` while it's on its way, `✓` once the other app has
it, or `✗` if it didn't arrive within 5 seconds (say they closed the lid).
Nothing is resent, so press ↑ to bring it back and send it again. The window
title shows how many messages you haven't read.

### Rooms

A room holds 2–13 people (`/create cat` asks how many; Enter means 13), and you
can be in up to 3 rooms at once. Its key is 6 characters made up by the app.
Anyone with the room's name and key can *ask* to join; the people already
inside see the request and approve or deny it. A request waits up to 2 minutes.

`/invite @name` sends someone an invitation: they answer with `/accept` or
`/decline`, and if they accept, your invite counts as your approval. An
invitation lasts 10 minutes.

A room's sidebar shows its key, how full it is and who's waiting to get in.
Click the room name or key to copy the line someone pastes to ask to join, for
anyone who isn't online yet. Copying uses your terminal's clipboard support (OSC 52); macOS
Terminal.app doesn't have it, so there you select the line with Option-drag
instead.

How many approvals a newcomer needs depends on how many people are in the room
(online) right now:

| In the room | Approvals needed |
|---|---|
| 1–3 | 1 |
| 4–6 | 2 |
| 7–9 | 3 |
| 10–12 | 4 |

One `/deny` turns the request away. If two people are let in for the last
place, whoever asked first gets it.

`/kick @name` votes someone out. It takes as many votes as letting someone in
(the table above). Someone kicked can ask to join again; someone kicked twice
can't get back into that room.

Whenever someone leaves a room, drops off or is kicked, the people still in it
switch to a new encryption key on their own, so whoever left can't read on.

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
corporate networks, some routers) block it. In that case, connect directly
using the ip and port shown on the other person's bottom line:

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
refused, so bump it (and `Cargo.lock`) in its own pull request to `main`
first. CI fails a pull request into `release` that still has a released
version, before it can be merged.

## License

MIT. See [LICENSE](LICENSE).
