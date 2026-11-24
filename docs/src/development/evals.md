# Evals

Unit tests check that memdusa's pieces work. The evals check that the whole thing does what it promises,
with real agents: that work interrupted in one session can be continued in another. They live in `eval/`
and are described in detail in
[eval/README.md](https://github.com/Krishnacore/memdusa/blob/main/eval/README.md).

**Note:** the evals run real agent sessions on your accounts. They take minutes and cost real money
(roughly a dollar or two per handoff trial on Sonnet). Free harnesses and models are used where possible.

## Handoff eval

`eval/handoff/run.py` interrupts agent A partway through a task, then lets agent B continue, with and
without memdusa, and grades the result with hidden tests.

```sh
uv run eval/handoff/run.py                       # 1 trial, Claude Code on Sonnet
uv run eval/handoff/run.py --trials 5 --interrupt-after 60
```

## Multi-agent eval

`eval/multiagent/run.py` checks the coordination machinery. A coordinator delegates two tasks as one
durable run, workers in different harnesses claim their steps, one worker is killed mid-task, and a
replacement in a third harness takes over the stale step from its brief.

```sh
uv run eval/multiagent/run.py --dry-run          # stub harnesses: free
uv run eval/multiagent/run.py --self-test        # grading unit checks
uv run eval/multiagent/run.py                    # 1 real trial
uv run eval/multiagent/run.py --matrix           # every harness in every role
uv run eval/multiagent/run.py --memdusa-bin ./memdusa --harnesses worker1=opencode,worker2=opencode,replacement=opencode
```

`--memdusa-bin` uses a prebuilt binary instead of `cargo build`, for example in a VM without Rust.
Hermes trials temporarily edit `~/.hermes/config.yaml` and only run with `--allow-user-config`. The
original is restored byte for byte.
