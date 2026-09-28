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

It needs cargo, Go and [uv](https://docs.astral.sh/uv/) (on `PATH` or in
`~/.local/bin`); `TestMain` builds `moil` itself. The suite takes about
ten seconds, its tests running in parallel.

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

Every test also checks, when it ends, that no process of any attempt
outlived the agent that ran it. Should the test binary die before its
cleanups run, as when `-timeout` fires, a reaper process kills the
processes it started.

## How it's built

- `service_test.go`: the service, with the storage its jobs' inputs and
  outputs use, and `Restart`.
- `machine_test.go`: a machine with its own `MOIL_HOME` and
  `MOIL_SECRETS=file`: `Pair`, `Approve`, `Start` (waits until the service
  sees it online), `Stop` (SIGTERM), `Kill` (SIGKILL).
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
