# Joining the bus

Five minutes, Windows or Linux. You need Rust (rustup) and a token from the hub owner, owner. Code still travels through git branches on GitHub. The bus carries short coordination lines only.

## 1. Get the binary

Once `bus` is in the main repository:

```sh
cd decant
cargo install --path bus
```

That puts `sommelier` (or `sommelier.exe`) into `~/.cargo/bin`, which is already on your PATH.

Until then the source is on the branch `bus` of the repository (`git fetch origin bus`, then `git checkout origin/bus -- bus`):

```sh
cd sommelier            # the folder with Cargo.toml
cargo build --release
# Linux:   ./target/release/sommelier
# Windows: .\target\release\sommelier.exe
```

or `cargo install --path .` from that folder. The folder pins its Rust version in `rust-toolchain.toml`, rustup fetches it on the first build. The build needs the usual C toolchain Rust already uses on your system (gcc or clang on Linux, the MSVC build tools on Windows).

Check: `sommelier version` prints `sommelier 0.1.0` or newer.

## 2. Point it at the hub

Ask owner for a token (privately). You get one line starting with `sb_`. Keep it out of every repository, issue and chat log. A token file is the safest place, because it does not show up in environment dumps.

Linux:

```sh
mkdir -p ~/.config/sommelier
printf '%s\n' 'sb_...' > ~/.config/sommelier/token && chmod 600 ~/.config/sommelier/token
echo 'export SOMMELIER_URL=wss://hub.example.com' >> ~/.bashrc
echo 'export SOMMELIER_TOKEN_FILE=$HOME/.config/sommelier/token' >> ~/.bashrc
```

Windows (new terminals pick it up):

```powershell
Set-Content -Path "$env:USERPROFILE\.sommelier-token" -Value 'sb_...'
setx SOMMELIER_URL wss://hub.example.com
setx SOMMELIER_TOKEN_FILE "$env:USERPROFILE\.sommelier-token"
```

`SOMMELIER_TOKEN=sb_...` works too, and so do `--url`, `--token` and `--token-file` on every command. A flag wins over the variable.

```sh
sommelier whoami
# alice member (hub at #1234)
```

If you see `unknown or revoked token`, ask the owner for a new one.

## 3. Watch

```sh
sommelier tail --last 20          # the last 20 messages, then live
```

Each message is one line: `#id time from KIND topic refs : body`, for example

```
#1234 20:05:11 alice READY issue/189 shaders/ssa-phi-placement 1a2b3c4 : phi count bounded
```

`tail` stays open, reconnects by itself and continues where it stopped. Useful filters:

```sh
sommelier tail --kind RUN_REQUEST,TASK        # only offers of work
sommelier tail --topic issue/*                # a topic prefix (quote it in bash: 'issue/*')
sommelier tail --from owner,alice         # only these senders
sommelier tail --json                         # the raw JSON line per message
sommelier log --last 50                       # one shot, prints and exits
sommelier claims                              # who holds which topic
sommelier who                                 # who is connected right now
```

Plain `sommelier tail` without `--last` or `--since` shows only messages that arrive after it started.

## 4. Send

```sh
sommelier send CHAT "hello, Linux box online"
# #1235
```

The general form is `sommelier send KIND [BODY] [--topic T] [--ref R]... [--re ID]...`.

* The body is one line, at most 4096 bytes. Put it in quotes as one argument. To pipe the body in, give `-` as the body (`... | sommelier send RUN_RESULT -`), lines are joined with ` | `. stdin is read only then (or with `--stdin`), so `send` never waits on stdin by surprise. Without a body only CLAIM, RELEASE and ACCEPT are sent, other kinds are refused.
* `--topic` is one word without spaces, for example `issue/189` or `run/decant-d3d12x`.
* `--ref` adds a reference without spaces (a branch, a commit id, a file). Repeat it, up to 16.
* `--re 1234` adds the reference `#1234`, the message you answer.
* The command prints the new message id.

The kinds are READY, LANDED, FIX, STUCK, FINDING, CLAIM, RELEASE, TASK, ACCEPT, DECLINE, RUN_REQUEST, RUN_RESULT, HELLO and CHAT. Only an orchestrator may send TASK. The README has a table with what each means.

## 5. Announce your machine

Send one HELLO when you join and again when the machine changes. It tells the others what you can run for them.

```sh
sommelier send HELLO "os=Arch Linux 6.12, gpu=RX 7900 XTX (Mesa 25.2, RADV), cpu=Ryzen 9 7950X, ram=64GB, agent=Hermes, can run: workspace tests, shader tests"
```

Describe hardware and capabilities only. No serial numbers, no user names, no paths from your home folder.

## 6. The two flows that matter first

### Reporting a branch as ready

Push the branch to GitHub first, then:

```sh
sommelier send READY "phi count bounded, workspace tests green on Linux" --topic issue/189 --ref shaders/ssa-phi-placement --ref 1a2b3c4
```

The orchestrator answers with LANDED once it is merged, or with FIX and the facts when something needs another look. Answers carry `#<your id>` as a reference, so `sommelier tail --from owner` or a look for your id finds them. The pull request and the issue on GitHub stay the record for humans.

Before working on a topic, claim it, and release it when you stop:

```sh
sommelier send CLAIM "taking the vertex fetch path" --topic issue/186
sommelier send RELEASE "done, see READY #1240" --topic issue/186
```

A claim on a topic someone else holds is refused with the holder's name. A claim lapses when its holder sends nothing for 60 minutes. Any message counts, and `sommelier heartbeat` keeps all your claims alive without a message.

### Answering a run request

Someone wants a test run on your hardware:

```
#1300 20:31:02 owner RUN_REQUEST @alice run/decant-d3d12x d3d12x/some-fix 9f8e7d6 : cargo test -p decant-d3d12x on an AMD GPU, report pass count
```

A request is an offer, not an order. Look at it with your own eyes (or let your agent check it against your rules), then either decline:

```sh
sommelier send DECLINE "not now" --re 1300 --topic run/decant-d3d12x
```

A DECLINE needs no reason, any short body will do. Or accept, run it yourself from the named commit on the GitHub repository, and report:

```sh
sommelier send ACCEPT "running on RX 7900 XTX" --re 1300 --topic run/decant-d3d12x
git fetch origin d3d12x/some-fix && git switch --detach 9f8e7d6
cargo test -p decant-d3d12x 2>&1 | tail -n 3 | sommelier send RUN_RESULT - --re 1300 --topic run/decant-d3d12x --ref 9f8e7d6
```

The last line sends the final three output lines as one message, joined with ` | `. Write what was measured (pass counts, times, the exact commit) and keep paths, user names and anything from the game files out of it.

`@alice` means the message is addressed to alice (sent with `--to alice`). Everyone sees it, and `sommelier tail --to me` shows only messages for everyone plus those addressed to you.

## Next

* `docs/AGENTS.md`: the text to paste into your agent's instructions, with the rules of conduct.
* Problems: `sommelier whoami` shows whether the URL and the token work. Ask on the tracking issue or in a CHAT.
