package moil_test

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"path/filepath"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

// Machine endpoints refuse requests without a valid token, before
// anything else happens.
func TestMachineEndpointsNeedAToken(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	base := moiltest.Serve(t, srv)
	for _, token := range []string{"", "moil_forged", strings.ToUpper(m.Token())} {
		for _, ep := range []struct{ method, path string }{
			{http.MethodGet, "/v1/bundles"},
			{http.MethodGet, "/v1/bundles/" + bundle.Hash()},
			{http.MethodGet, "/v1/connect"},
			{http.MethodDelete, "/v1/machine"},
		} {
			status, body := request(t, ep.method, base+ep.path, token, nil)
			if status != http.StatusUnauthorized || body["error"] != "unauthorized" {
				t.Errorf("%s %s with token %q: %d %v", ep.method, ep.path, token, status, body)
			}
		}
	}
	if _, err := srv.Machine(context.Background(), m.ID()); err != nil {
		t.Fatalf("the machine must survive forged requests: %v", err)
	}
}

// Removing a machine revokes its token and closes its channel with 4401,
// so the app stops reconnecting and shows the service as unpaired.
func TestRemovingAMachineRevokesItsToken(t *testing.T) {
	srv := newServer(t)
	m := moiltest.Pair(t, srv, "alice")
	m.Connect()
	if err := srv.RemoveMachine(context.Background(), m.ID()); err != nil {
		t.Fatal(err)
	}
	if code := m.WaitClosed(); code != 4401 {
		t.Fatalf("closed with %d, want 4401", code)
	}
	if err := m.TryConnect(); !errors.Is(err, moiltest.ErrUnauthorized) {
		t.Fatalf("reconnecting with a revoked token: %v", err)
	}
	if status, _ := request(t, http.MethodGet, moiltest.Serve(t, srv)+"/v1/bundles", m.Token(), nil); status != http.StatusUnauthorized {
		t.Fatalf("GET /v1/bundles with a revoked token: %d", status)
	}
	if _, err := srv.Machine(context.Background(), m.ID()); !errors.Is(err, moil.ErrNotFound) {
		t.Fatalf("Machine after removal: %v", err)
	}
	if err := srv.RemoveMachine(context.Background(), m.ID()); !errors.Is(err, moil.ErrNotFound) {
		t.Fatalf("removing twice: %v", err)
	}
}

// A machine that unpairs itself is forgotten by the service too.
func TestUnpairFromTheMachine(t *testing.T) {
	srv := newServer(t)
	m := moiltest.Pair(t, srv, "alice")
	m.Connect()
	m.Unpair()
	if ms, err := srv.Machines(context.Background(), "alice"); err != nil || len(ms) != 0 {
		t.Fatalf("Machines after unpairing: %+v, %v", ms, err)
	}
	if err := m.TryConnect(); !errors.Is(err, moiltest.ErrUnauthorized) {
		t.Fatalf("connecting after unpairing: %v", err)
	}
}

// A second connection with the same token replaces the first, which is
// closed with 4409 so the two copies of the app don't fight.
func TestNewConnectionReplacesTheOld(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	first := moiltest.Pair(t, srv, "alice")
	first.Approve(bundle)
	first.Connect()
	second := first.Twin()
	second.Connect()
	if code := first.WaitClosed(); code != 4409 {
		t.Fatalf("the first connection closed with %d, want 4409", code)
	}
	if !machine(t, srv, first.ID()).Online() {
		t.Fatal("the machine must stay online through its new connection")
	}
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	second.NextAttempt().Succeed(nil)
	succeeded(t, run)
}

// A machine that sends something that isn't a valid message is
// disconnected with 4400; messages from newer machines are ignored.
func TestProtocolErrorsClose4400(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	for _, c := range []struct {
		name string
		msg  any
	}{
		{"not an object", "hello"},
		{"no type", map[string]any{"job_id": "j"}},
		{"invalid job ID", map[string]any{"type": "bid", "job_id": "../x"}},
		{"unknown state", map[string]any{"type": "state", "state": "napping"}},
		{"a second hello", map[string]any{"type": "hello", "protocol": 1, "app_version": "0.1.0",
			"machine": map[string]any{"name": "x", "os": "linux", "arch": "x86_64", "cpus": 1, "memory_bytes": 1, "gpus": []any{}},
			"state":   "idle", "approved": []any{}, "attempts": []any{}}},
		{"a service message", map[string]any{"type": "ack", "job_id": "j", "attempt": 1}},
		// The limits of spec §10: a machine can't make the service hold
		// more than they allow.
		{"a log message over 64 KiB", event(map[string]any{"type": "log", "message": strings.Repeat("x", 64<<10+1)})},
		{"a data payload over 256 KiB", event(map[string]any{"type": "data", "payload": strings.Repeat("x", 256<<10)})},
		{"a data payload nested too deep", event(map[string]any{"type": "data", "payload": json.RawMessage(strings.Repeat("[", 65) + strings.Repeat("]", 65))})},
		{"a progress fraction above 1", event(map[string]any{"type": "progress", "fraction": 1.5})},
		{"result meta over 256 KiB", map[string]any{"type": "done", "job_id": "j", "attempt": 1, "seq": 1, "outcome": "succeeded",
			"result": map[string]any{"files": map[string]any{}, "meta": strings.Repeat("x", 256<<10)}}},
		{"a stderr tail over 8 KiB", map[string]any{"type": "done", "job_id": "j", "attempt": 1, "seq": 1, "outcome": "failed",
			"error": map[string]any{"code": "exit", "message": "", "retryable": false, "stderr_tail": strings.Repeat("x", 8<<10+1)}}},
		{"more approvals than a service keeps", map[string]any{"type": "approved", "hashes": slices.Repeat([]string{strings.Repeat("a", 64)}, 1025)}},
	} {
		t.Run(c.name, func(t *testing.T) {
			m := moiltest.Pair(t, srv, "alice")
			m.Connect()
			m.Send(c.msg)
			if code := m.WaitClosed(); code != 4400 {
				t.Fatalf("closed with %d, want 4400", code)
			}
		})
	}

	t.Run("additions are ignored", func(t *testing.T) {
		m := moiltest.Pair(t, srv, "alice")
		m.Connect()
		m.Send(map[string]any{"type": "telemetry", "gpu_temp": 71})
		m.Send(map[string]any{"type": "state", "state": "paused", "battery": 0.4})
		m.Approve(bundle)
		m.Sync()
		got := machine(t, srv, m.ID())
		if !got.Online() || got.State != moil.Paused || !got.HasApproved(bundle.Hash()) {
			t.Fatalf("after messages with additions: %+v", got)
		}
	})
}

func event(e map[string]any) map[string]any {
	return map[string]any{"type": "event", "job_id": "j", "attempt": 1, "seq": 1, "event": e}
}

// The first message must be a valid hello; anything else is closed with
// 4400 before the machine counts as online.
func TestHelloComesFirst(t *testing.T) {
	srv := newServer(t)
	m := moiltest.Pair(t, srv, "alice")
	base := moiltest.Serve(t, srv)
	for _, first := range []string{
		`{"type": "state", "state": "idle"}`,
		`{"type": "hello", "protocol": 1, "app_version": "0.1.0", "state": "idle", "approved": [], "attempts": [],
		  "machine": {"name": "` + strings.Repeat("x", 65) + `", "os": "linux", "arch": "x86_64", "cpus": 1, "memory_bytes": 1, "gpus": []}}`,
	} {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		conn, _, err := websocket.Dial(ctx, base+"/v1/connect", &websocket.DialOptions{
			HTTPHeader: http.Header{"Authorization": {"Bearer " + m.Token()}},
		})
		if err != nil {
			t.Fatal(err)
		}
		defer conn.CloseNow()
		if err := conn.Write(ctx, websocket.MessageText, []byte(first)); err != nil {
			t.Fatal(err)
		}
		_, _, err = conn.Read(ctx)
		if code := websocket.CloseStatus(err); code != 4400 {
			t.Fatalf("%.40s…: closed with %d (%v), want 4400", first, code, err)
		}
		if machine(t, srv, m.ID()).Online() {
			t.Fatal("a machine that never said a valid hello is online")
		}
	}
}

// A machine can't make the service hold unbounded messages for it: one
// that keeps the service sending while it never reads is disconnected
// once a bounded queue fills, long before a write would time out.
func TestMachinesThatDontReadAreDisconnected(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.PingInterval = 20 * time.Second })
	m := moiltest.Pair(t, srv, "alice")
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	conn, _, err := websocket.Dial(ctx, moiltest.Serve(t, srv)+"/v1/connect", &websocket.DialOptions{
		HTTPHeader: http.Header{"Authorization": {"Bearer " + m.Token()}},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer conn.CloseNow()
	hello := `{"type": "hello", "protocol": 1, "app_version": "0.1.0", "state": "idle", "approved": [], "attempts": [],
		"machine": {"name": "m", "os": "linux", "arch": "x86_64", "cpus": 1, "memory_bytes": 1, "gpus": []}}`
	if err := conn.Write(ctx, websocket.MessageText, []byte(hello)); err != nil {
		t.Fatal(err)
	}
	// Every done is acknowledged; this machine never reads the acks. Long
	// job IDs make long acks, which fill the connection's buffers sooner.
	jobID := strings.Repeat("j", 128)
	for attempt := 1; ; attempt++ {
		done := fmt.Sprintf(`{"type": "done", "job_id": "%s", "attempt": %d, "seq": 1, "outcome": "cancelled"}`, jobID, attempt)
		if err := conn.Write(ctx, websocket.MessageText, []byte(done)); err != nil {
			if ctx.Err() != nil {
				t.Fatalf("still connected after %d unread acks", attempt)
			}
			break
		}
	}
	eventually(t, "the machine to go offline", func() bool { return !machine(t, srv, m.ID()).Online() })
}

// Machines lists what each machine last reported, live while connected and
// from the Store once it disconnects, even across a service restart.
func TestMachineRegistry(t *testing.T) {
	storePath := filepath.Join(t.TempDir(), "machines.json")
	store, err := moil.NewFileStore(storePath)
	if err != nil {
		t.Fatal(err)
	}
	srv := newServer(t, func(c *moil.Config) { c.Store = store })
	// The service's address outlives the Server, as a restarted
	// service's does.
	var front swapHandler
	front.set(srv.Handler())
	base := httptestServer(t, &front)
	bundle := loadBundle(t, srv, "minimal")
	ada := moiltest.Pair(t, srv, "alice", moiltest.At(base), moiltest.WithName("Ada's desktop"), moiltest.WithHardware(16, 64<<30), moiltest.WithGPU("RTX 4090", 24<<30))
	moiltest.Pair(t, srv, "alice", moiltest.At(base), moiltest.WithName("Ada's laptop"))
	moiltest.Pair(t, srv, "bob", moiltest.At(base))

	ada.Approve(bundle)
	ada.Connect()
	ms, err := srv.Machines(context.Background(), "alice")
	if err != nil {
		t.Fatal(err)
	}
	if len(ms) != 2 {
		t.Fatalf("alice has %d machines, want 2 (and none of bob's)", len(ms))
	}
	desktop, laptop := ms[0], ms[1]
	if desktop.ID != ada.ID() || desktop.Name != "Ada's desktop" || !desktop.Online() || desktop.State != moil.Idle ||
		desktop.CPUs != 16 || desktop.MemoryBytes != 64<<30 ||
		!slices.Equal(desktop.GPUs, []moil.GPU{{Name: "RTX 4090", MemoryBytes: 24 << 30}}) ||
		!desktop.HasApproved(bundle.Hash()) || desktop.AppVersion != "moiltest" {
		t.Errorf("connected machine %+v", desktop)
	}
	if laptop.Name != "Ada's laptop" || laptop.Online() || laptop.State != moil.Offline {
		t.Errorf("offline machine %+v", laptop)
	}

	ada.SetState(moil.Paused)
	ada.Sync()
	if got := machine(t, srv, ada.ID()); got.State != moil.Paused {
		t.Errorf("after pausing: %s", got.State)
	}
	before := time.Now()
	ada.Disconnect()
	eventually(t, "the machine to go offline", func() bool { return !machine(t, srv, ada.ID()).Online() })
	srv.Close()

	// A restarted service knows the machine as it last reported.
	reopened, err := moil.NewFileStore(storePath)
	if err != nil {
		t.Fatal(err)
	}
	srv = newServer(t, func(c *moil.Config) { c.Store = reopened })
	front.set(srv.Handler())
	got := machine(t, srv, ada.ID())
	if got.Online() || got.Name != "Ada's desktop" || !got.HasApproved(bundle.Hash()) || got.CPUs != 16 || got.LastSeen.Before(before) {
		t.Fatalf("after a restart: %+v", got)
	}
	ada.Connect()
	if !machine(t, srv, ada.ID()).Online() {
		t.Fatal("the machine can't reconnect after a service restart")
	}
}

// swapHandler serves whichever handler was set last.
type swapHandler struct {
	h atomic.Pointer[http.Handler]
}

func (s *swapHandler) set(h http.Handler) { s.h.Store(&h) }

func (s *swapHandler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	(*s.h.Load()).ServeHTTP(w, r)
}

// The service pings every machine and drops one that stops answering.
func TestUnresponsiveMachinesAreDisconnected(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.PingInterval = 20 * time.Millisecond })
	healthy := moiltest.Pair(t, srv, "alice")
	hung := moiltest.Pair(t, srv, "alice")
	healthy.Connect()
	hung.Connect()
	hung.IgnorePings()
	eventually(t, "the hung machine to be dropped", func() bool { return !machine(t, srv, hung.ID()).Online() })
	eventually(t, "a few pings", func() bool { return healthy.Pings() >= 3 })
	if !machine(t, srv, healthy.ID()).Online() {
		t.Fatal("a machine answering pings was dropped")
	}
}

// Bundles reach machines in welcome, as soon as they're added, and over
// HTTP, where the machine can verify them.
func TestBundlesReachMachines(t *testing.T) {
	srv := newServer(t)
	minimal := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Connect()
	if got := m.Bundles(); len(got) != 1 || got[0].Hash != minimal.Hash() || got[0].Name != "minimal" || got[0].Version != "1.0.0" || got[0].Description == "" {
		t.Fatalf("welcome bundles %+v", got)
	}

	unicode := vectorBundle(t, "crlf-unicode")
	srv.AddBundle(unicode)
	m.WaitForBundle(unicode.Hash())
	srv.AddBundle(unicode, minimal) // nothing new: no message
	m.Sync()
	if got := m.Bundles(); len(got) != 2 {
		t.Fatalf("advertised %+v", got)
	}

	listed := m.ListBundles()
	if len(listed) != 2 || listed[0].Hash != minimal.Hash() || listed[1].Hash != unicode.Hash() {
		t.Fatalf("GET /v1/bundles: %+v", listed)
	}
	fetched := m.FetchBundle(unicode.Hash())
	if string(fetched.File(moil.ScriptFile)) != string(unicode.File(moil.ScriptFile)) {
		t.Fatal("the fetched script differs")
	}
	status, body := request(t, http.MethodGet, moiltest.Serve(t, srv)+"/v1/bundles/"+strings.Repeat("0", 64), m.Token(), nil)
	if status != http.StatusNotFound || body["error"] != "not_found" {
		t.Fatalf("unknown bundle: %d %v", status, body)
	}
}

// Closing the Server tells machines it's going away and ends unfinished
// runs; nothing more can be submitted.
func TestCloseEndsRunsAndConnections(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	running := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	m.NextAttempt()
	queued := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	srv.Close()
	if code := m.WaitClosed(); code != int(websocket.StatusGoingAway) {
		t.Errorf("closed with %d, want 1001", code)
	}
	for _, run := range []*moil.Run{running, queued} {
		if _, err := wait(t, run); !errors.Is(err, moil.ErrClosed) {
			t.Errorf("run %s ended with %v, want ErrClosed", run.ID(), err)
		}
	}
	if _, err := srv.Submit(context.Background(), moil.Job{Bundle: bundle, Eligible: moil.AnyMachine}); !errors.Is(err, moil.ErrClosed) {
		t.Errorf("Submit after Close: %v", err)
	}
}

// Control messages are limited to 1 MiB (spec §10); a machine that sends
// a bigger one is disconnected rather than buffered.
func TestOversizedMessagesAreRefused(t *testing.T) {
	srv := newServer(t)
	m := moiltest.Pair(t, srv, "alice")
	m.Connect()
	m.Send(map[string]any{"type": "telemetry", "blob": strings.Repeat("x", 1<<20)})
	if code := m.WaitClosed(); code != int(websocket.StatusMessageTooBig) {
		t.Fatalf("closed with %d, want %d", code, websocket.StatusMessageTooBig)
	}
}

// A connection that never says hello doesn't stay open.
func TestSilentConnectionsAreClosed(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.PingInterval = 50 * time.Millisecond })
	m := moiltest.Pair(t, srv, "alice")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, _, err := websocket.Dial(ctx, moiltest.Serve(t, srv)+"/v1/connect", &websocket.DialOptions{
		HTTPHeader: http.Header{"Authorization": {"Bearer " + m.Token()}},
	})
	if err != nil {
		t.Fatal(err)
	}
	defer conn.CloseNow()
	if _, _, err := conn.Read(ctx); ctx.Err() != nil {
		t.Fatalf("the connection stayed open without a hello: %v", err)
	}
}
