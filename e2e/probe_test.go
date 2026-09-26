package e2e

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// The probe is the bundle these tests run (testdata/bundles/probe): a
// script that takes the steps its params list. Tests spell the steps out
// with do, so each test says what the script does:
//
//	Params: probe(do.Progress(0.5, "halfway"), do.Result(nil))

func loadProbe(t *testing.T) *moil.Bundle {
	t.Helper()
	b, err := moil.LoadBundle(os.DirFS("testdata/bundles/probe"))
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// probeParams are the probe's params.
type probeParams struct {
	// Attempts lists the steps of each attempt; the last list repeats.
	Attempts [][]step `json:"attempts"`
	// Record is where the probe records its process group; see
	// service.Submit.
	Record string `json:"record,omitempty"`
}

// probe returns params that make every attempt take steps.
func probe(steps ...step) probeParams {
	return probeParams{Attempts: [][]step{steps}}
}

// probeAttempts returns params that make the n-th attempt take the n-th
// list of steps, and later attempts the last.
func probeAttempts(attempts ...[]step) probeParams {
	return probeParams{Attempts: attempts}
}

// A step is one thing the probe does.
type step map[string]any

type actions struct{}

// do spells out the probe's steps.
var do actions

// Progress sends a progress event.
func (actions) Progress(fraction float64, message string) step {
	return step{"progress": fraction, "message": message}
}

// Data sends a data event carrying payload.
func (actions) Data(payload any) step { return step{"data": payload} }

// Count sends data events {"n": first} to {"n": last}.
func (actions) Count(first, last int) step { return step{"count": []int{first, last}} }

// Junk prints a line that isn't an event.
func (actions) Junk(line string) step { return step{"junk": line} }

// Stderr writes a line to stderr.
func (actions) Stderr(line string) step { return step{"stderr": line} }

// Copy copies an input file to an output file.
func (actions) Copy(input, output string) step { return step{"copy": input, "to": output} }

// Reveal sends what the script can see: its environment and the job
// context (see awaitRevealed).
func (actions) Reveal() step { return step{"reveal": true} }

// Spawn starts a grandchild that ignores SIGTERM, and sends the pids of
// the processes running the attempt (see awaitPids).
func (actions) Spawn() step { return step{"spawn": true} }

// WaitFor waits until g is open.
func (actions) WaitFor(g gate) step { return step{"wait": string(g)} }

// Open opens g.
func (actions) Open(g gate) step { return step{"touch": string(g)} }

// Sleep sleeps.
func (actions) Sleep(d time.Duration) step { return step{"sleep": d.Seconds()} }

// Fail sends an error event.
func (actions) Fail(message string, retryable bool) step {
	return step{"fail": message, "retryable": retryable}
}

// Exit exits with code.
func (actions) Exit(code int) step { return step{"exit": code} }

// Result sends a result event naming the output files to upload.
func (actions) Result(meta any, files ...string) step {
	if files == nil {
		files = []string{}
	}
	return step{"result": map[string]any{"files": files, "meta": meta}}
}

// A gate is a file that the probe or a test creates to let the other one
// carry on.
type gate string

func newGate(t *testing.T) gate {
	return gate(filepath.Join(t.TempDir(), "gate"))
}

// Open lets whoever waits for g carry on.
func (g gate) Open(t *testing.T) {
	t.Helper()
	if err := os.WriteFile(string(g), nil, 0o644); err != nil {
		t.Fatal(err)
	}
}

// Await waits until g is open.
func (g gate) Await(t *testing.T, what string) {
	t.Helper()
	eventually(t, what, func() bool {
		_, err := os.Stat(string(g))
		return err == nil
	})
}

// pids are the processes running an attempt of the probe, as it reports
// them after starting its grandchild.
type pids struct {
	UV         int `json:"uv"`
	Script     int `json:"script"`
	Grandchild int `json:"grandchild"`
}

func (p pids) all() []int { return []int{p.UV, p.Script, p.Grandchild} }

// awaitPids waits for the given attempt of the probe to report its pids.
func awaitPids(t *testing.T, run *moil.Run, attempt int) pids {
	t.Helper()
	var reported struct {
		Pids *pids `json:"pids"`
	}
	awaitEvent(t, run, "the probe's pids", func(ev moil.Event) bool {
		return ev.Kind == moil.EventData && ev.Attempt == attempt &&
			json.Unmarshal(ev.Data, &reported) == nil && reported.Pids != nil
	})
	for _, pid := range reported.Pids.all() {
		if !alive(pid) {
			t.Fatalf("the probe reported pid %d, which isn't running", pid)
		}
	}
	return *reported.Pids
}
