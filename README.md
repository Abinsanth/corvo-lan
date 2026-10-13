# lan-chat

Serverless chat for your local network. Run it on two machines on the same
Wi-Fi or LAN and they find each other. There's no server, no account and no
setup.

- People find each other automatically (UDP multicast), then talk over direct TCP connections.
- Everyone gets a unique name: you choose the start and a word is added (`--nick bob` → `@bobRoger`).
- Private rooms are protected by a key and end-to-end encrypted. Only people with the room's name and key can read it.
- Direct messages go peer to peer. They are **not encrypted yet**; that comes in v0.4.
- Chats are never written to disk. Quitting forgets everything.

## Install

You need a Rust toolchain ([rustup.rs](https://rustup.rs)).

```bash
git clone https://github.com/Abinsanth/lan-chat.git
cd lan-chat
cargo install --path .
```

## Use

```bash
lan-chat                   # your name starts with $USER
lan-chat --nick bob        # or choose the start yourself
```

After about two seconds you get a name, and you're connected to everyone else running it.

```
/create cat 123            # creates e.g. #catPieR with key 123-k7m9xq
/chat #catPieR 123-k7m9xq  # someone else joins with the name and key
/chat @bobRoger            # private chat
/msg @bobRoger hi          # one-off private message
/who                       # who's connected
/help                      # all commands
```

Plain text goes to the conversation shown in the prompt.

### If peers don't find each other

Discovery uses multicast on `239.255.42.99:4299`. Some networks (guest Wi-Fi,
corporate networks, some routers) block it. In that case, connect directly:

```bash
lan-chat --port 5000                      # on one machine
lan-chat --peer 192.168.1.20:5000         # on the other
```

Your firewall must also allow incoming TCP on the port you use. `--debug`
writes a log to `./lan-chat.log`.

## Development

```bash
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

CI runs all three on every pull request.
