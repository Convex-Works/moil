# End-to-end tests

The whole of moil, as its users run it: a service built on the Go SDK
(`moil.NewServer` behind an `httptest` server on 127.0.0.1), machines
running the real `moil` binary built from this repository (`moil pair`,
`moil approve`, `moil agent`), and real uv running a real bundle, with
real processes and sockets in between.

```
make e2e                      # builds moil, then runs the suite
cd e2e && go test -race ./... # once moil is built
```

It needs cargo, Go, git and [uv](https://docs.astral.sh/uv/) (on `PATH`
or in `~/.local/bin`); `TestMain` builds `moil` itself. The suite takes
about twelve seconds, its tests running in parallel.

## What it covers

Each test is a story of what a service or a machine's owner does, and
what must hold afterwards:

| Test | Story |
|---|---|
| `TestAJobRunsOnAPairedMachine` | Pair, approve, run: phases, progress and data arrive in order and intact, the input comes down and the output goes up with the headers the job gave, and the result describes the uploaded bytes |
| `TestMachinesRunOnlyWhatTheirOwnerApproved` | A job for an unapproved bundle never reaches the machine; the owner follows the agent's hint and approves while the agent runs, and the job runs |
| `TestTamperedBundlesAreRefused` | A bundle altered on its way is refused by `moil review` and `moil approve`, and never runs |
| `TestScriptsSeeOnlyTheirJob` | The script sees its files, but not the machine's token, the service's URLs or the agent's settings |
| `TestFailuresReachTheService` | Script errors (retried up to the limit, or not), exiting without a result, a nonzero exit, the time limit |
| `TestCancellingAJobStopsEverythingItStarted` | Cancel ends the script and a grandchild that ignores SIGTERM |
| `TestAJobSurvivesItsMachineCrashing` | SIGKILL the agent: the lease runs out, the next agent kills the orphans, the job completes on attempt 2 |
| `TestStoppingTheAgentHandsTheJobBack` | SIGTERM the agent: `interrupted` reaches the service at once, the job resumes later |
| `TestAJobSurvivesTheNetworkDropping` | Cut the network mid-job: the agent reconnects and replays, the service sees every event once, in order |
| `TestAServiceRestartDropsTheJobsItForgot` | Restart the service: machines drop its forgotten attempts, a resubmitted job runs |
| `TestJobsGoOnlyToEligibleMachines` | alice's job never reaches bob's machine, though it's idle, approved and preferred |
| `TestRemovingAMachineCutsItOff` | `RemoveMachine`: the attempt is lost, its processes end, the agent stops reconnecting |
| `TestTheExampleServiceWorksWithMoil` | `examples/service` with `examples/bundles/echo`, through its own pages |
| `TestAHookTurnsAJobsOutputIntoTheOwners` | A shell hook gets the attempt's context on stdin, field by field, the uploaded bytes in a folder of their own, and only the environment the spec lists; its log gets its output between moil's lines; its folders are removed; the service saw the job succeed first, and nothing of the hook |
| `TestHooksRunOneAtATimeInTheOrderTheirJobsEnded` | The machine takes jobs while a hook runs; hooks run one after another, in order, even paused, and none for a failed job; `moil logs -f` follows the hook's log |
| `TestMoilRunsOnlyHooksThatSayWhatRunsThem` | A hook with neither `#!` nor a program isn't run, and says why in its log, `moil hooks` and `moil status`; linked to a `uv run --script` Python file, it runs |
| `TestAFailedHookIsntRunAgain` | A hook that exits 3, or is killed, is logged so and not run again, even after a restart |
| `TestAHookSurvivesItsMachineCrashing` | SIGKILL the agent mid-hook: the next agent kills what's left, a SIGTERM-proof child included, logs `== stopped: moil stopped unexpectedly`, and runs the hook again with the same context and outputs; `moil quit` mid-hook likewise |
| `TestAnOwnersAgentDrivesMoilThroughItsCommandLine` | With `--json` only: pair, status while a job runs and while the service is out of reach, approve while the agent runs, pause and resume, stop, revoke, unpair and pair again, review with what changed, quit; every document in the spec's shapes, `status --watch` included, and exit status 3 without an agent |
| `TestAPauseMoilCantKeepFailsWithAnAgentToo` | With `settings.json` invalid, `moil pause` through a running agent exits with status 1 and says why, as it does without an agent |
| `TestAServiceFolderKeptInGitMovesToAnotherMachine` | Git never sees `pairing.json` or logs; a clone in a second home, paired by the service's ID, brings the approvals and hooks, and the second machine takes jobs without approving again |
| `TestGitPullsChangeWhatARunningMachineRuns` | A running agent pairs with a cloned folder; a pulled approval reaches the service within five seconds and the machine bids for its job; a pulled revocation interrupts the running attempt |
| `TestAnEditedBundleRunsNoMoreUntilGitRestoresIt` | Editing an approved `job.py` is reported, the approval is withdrawn from the service and its jobs wait, until `git checkout` restores the file |
| `TestTheWholeHomeCanBeKeptInGit` | In a repository at the root of the home, git sees only the owner's files |

Every test also checks, when it ends, that no process of any attempt,
or of the tests' hooks, outlived the agent that ran it. Should the test binary die before its
cleanups run, as when `-timeout` fires, a reaper process kills the
processes it started.

## How it's built

- `service_test.go`: the service, with the storage its jobs' inputs and
  outputs use, and `Restart`.
- `machine_test.go`: a machine with its own `MOIL_HOME` and
  `MOIL_SECRETS=file`: `Pair`, `PairJSON`, `Approve`, `Start` (waits until
  the service sees it online), `Stop` (SIGTERM), `Kill` (SIGKILL),
  `Exited` (after `moil quit`), and `follow` for commands that keep
  printing.
- `shapes_test.go`: the JSON of spec/machine.md §2.2 and §3.3 as Go
  structs. `decodeStrict` fails on a field the spec doesn't have, on one
  it requires that's missing, and on a null.
- `hooks_test.go`: `InstallHook`, a shell hook that saves its stdin,
  working directory, environment and outputs where the test reads them,
  then waits for a gate.
- `git_test.go`: real git, with none of the developer's configuration.
- `link_test.go`: the network in front of the service, which machines
  pair and connect through: a TCP proxy that can be cut, and that
  remembers what the service said to each machine.
- `probe_test.go` and `testdata/bundles/probe`: the bundle the tests run.
  Its params list steps (progress, data, junk output, copying an input to
  an output, revealing what it sees, spawning a SIGTERM-proof grandchild,
  waiting for a file, failing, exiting…), which tests spell out with `do`:
  `probe(do.Spawn(), do.WaitFor(gate))`.

Tests wait for conditions with deadlines, never for fixed times, except
where they check that something doesn't happen. Machines get uv through
a wrapper that gives it a cache of the suite's own, removed afterwards,
since the runtime passes uv only a minimal environment.
