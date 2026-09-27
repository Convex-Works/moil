package moil_test

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"math/big"
	"net/http"
	"slices"
	"strings"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

// A job goes from Submit to Result through a real machine connection: the
// machine gets everything the job declared, moves the files through
// storage, and its events and result reach the service.
func TestJobRunsEndToEnd(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	storage := newFileServer(t, map[string][]byte{"/rec/42.ogg": []byte("OggS…audio")})
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()

	run := submit(t, srv, moil.Job{
		ID:       "meeting-42-transcript",
		Bundle:   bundle,
		Title:    "Weekly sync",
		Params:   map[string]any{"language": "en"},
		Inputs:   map[string]moil.Download{"audio.ogg": {URL: storage.URL + "/rec/42.ogg"}},
		Outputs:  map[string]moil.Upload{"transcript.json": {URL: storage.URL + "/out/42.json", Headers: map[string]string{"Content-Type": "application/json"}}},
		Eligible: moil.OwnedBy("alice"),
		Timeout:  time.Hour,
	})
	if run.ID() != "meeting-42-transcript" {
		t.Fatalf("run ID %q", run.ID())
	}

	a := m.NextAttempt()
	if a.JobID != run.ID() || a.Number != 1 || a.BundleHash != bundle.Hash() || a.Title != "Weekly sync" {
		t.Fatalf("assignment: %+v", a)
	}
	var params map[string]string
	if err := json.Unmarshal(a.Params, &params); err != nil || params["language"] != "en" {
		t.Fatalf("params %s: %v", a.Params, err)
	}
	if a.Timeout != time.Hour || a.Lease != time.Second {
		t.Fatalf("timeout %v, lease %v", a.Timeout, a.Lease)
	}
	if run.State() != moil.Running {
		t.Fatalf("state %s while an attempt runs", run.State())
	}

	a.Phase("downloading")
	audio := a.Input("audio.ogg")
	a.Phase("running")
	a.Progress(0.5, "transcribing")
	a.Log("warn", "falling back to CPU")
	a.Data(map[string]any{"segment": map[string]any{"text": "hello"}})
	transcript := []byte(`{"text": "` + string(audio) + `"}`)
	a.Phase("uploading")
	a.Output("transcript.json", transcript)
	a.Succeed(map[string]any{"speakers": 2})

	res := succeeded(t, run)
	if res.Machine != m.ID() || res.Attempt != 1 {
		t.Errorf("result from %s attempt %d", res.Machine, res.Attempt)
	}
	if string(res.Meta) != `{"speakers":2}` {
		t.Errorf("meta %s", res.Meta)
	}
	sum := sha256.Sum256(transcript)
	if f := res.Files["transcript.json"]; f.SizeBytes != int64(len(transcript)) || f.SHA256 != hex.EncodeToString(sum[:]) {
		t.Errorf("file %+v", f)
	}
	stored, contentType := storage.file("/out/42.json")
	if string(stored) != string(transcript) || contentType != "application/json" {
		t.Errorf("storage got %q as %q", stored, contentType)
	}
	if run.State() != moil.Succeeded {
		t.Errorf("state %s", run.State())
	}

	evs := events(t, run)
	want := []moil.EventKind{
		moil.EventQueued, moil.EventAssigned,
		moil.EventPhase, moil.EventPhase, moil.EventProgress, moil.EventLog, moil.EventData, moil.EventPhase,
	}
	if !slices.Equal(kinds(evs), want) {
		t.Fatalf("events %v, want %v", kinds(evs), want)
	}
	if ev := evs[1]; ev.Machine != m.ID() || ev.Attempt != 1 {
		t.Errorf("assigned event %+v", ev)
	}
	if ev := evs[4]; ev.Fraction == nil || *ev.Fraction != 0.5 || ev.Message != "transcribing" || ev.Machine != m.ID() {
		t.Errorf("progress event %+v", ev)
	}
	if ev := evs[5]; ev.Level != "warn" || ev.Message != "falling back to CPU" {
		t.Errorf("log event %+v", ev)
	}
	if ev := evs[6]; string(ev.Data) != `{"segment":{"text":"hello"}}` {
		t.Errorf("data event %s", ev.Data)
	}
	a.WaitAcked()
}

// Params, data and meta travel as JSON values: machines re-encode them,
// and moiltest does as they do. What a machine wouldn't forward never
// reaches the service.
func TestJSONTravelsAsValuesNotBytes(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Params: json.RawMessage(`{"b": 1.0, "a": [1e2, 3]}`)})
	a := m.NextAttempt()
	if string(a.Params) != `{"a":[100.0,3],"b":1.0}` {
		t.Errorf("the script would read the params as %s", a.Params)
	}
	a.Data(json.RawMessage(`{"z": 1, "y": 2.50}`))
	a.Data(strings.Repeat("x", 256<<10))
	a.Succeed(map[string]any{"deep": json.RawMessage(strings.Repeat("[", 64) + strings.Repeat("]", 64))})
	_, err := wait(t, run)
	var jerr *moil.JobError
	if !errors.As(err, &jerr) || jerr.Code != moil.CodeNoResult {
		t.Errorf("a result with meta a machine refuses ended the job with %v, want %s", err, moil.CodeNoResult)
	}
	evs := events(t, run)
	if len(evs) != 4 || string(evs[2].Data) != `{"y":2.5,"z":1}` || evs[3].Kind != moil.EventLog || evs[3].Level != "warn" {
		t.Fatalf("events %+v", evs)
	}
}

// Phases and log levels are only shown, so the ones a newer machine
// sends reach the service as they are, rather than ending the channel.
func TestNewerPhasesAndLogLevelsPassThrough(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	a := m.NextAttempt()
	a.Phase("compiling")
	a.Log("trace", "linking")
	a.Succeed(nil)
	succeeded(t, run)
	evs := events(t, run)
	if len(evs) != 4 || evs[2].Phase != "compiling" || evs[3].Level != "trace" || evs[3].Message != "linking" {
		t.Fatalf("events %+v", evs)
	}
	if !m.Connected() {
		t.Fatal("the service closed the channel")
	}
}

// Submitting a job the Server knows returns the same Run, while it runs and
// after it ended, so a service can resubmit by ID without doubling work.
func TestSubmitIsIdempotentByID(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	job := moil.Job{ID: "meeting-7-summary", Bundle: bundle, Eligible: moil.AnyMachine}
	first := submit(t, srv, job)
	job.Title = "a different definition"
	if again := submit(t, srv, job); again != first {
		t.Fatal("resubmitting a queued job made a second run")
	}
	if got, ok := srv.Run("meeting-7-summary"); !ok || got != first {
		t.Fatal("Run doesn't find the job by ID")
	}

	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	a := m.NextAttempt()
	if a.Title != "" {
		t.Errorf("the second submission changed the job: title %q", a.Title)
	}
	a.Succeed(nil)
	succeeded(t, first)
	if again := submit(t, srv, job); again != first {
		t.Fatal("resubmitting a finished job made a second run")
	}
	other := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	if other == first || other.ID() == "" {
		t.Fatal("a job without an ID must get a new one")
	}
	m.Sync()
	if n := len(m.Offers()); n != 2 {
		t.Errorf("the machine got %d offers; resubmissions must not be offered again", n)
	}
}

// Finished jobs are forgotten after KeepFinished; a resubmission then runs
// again.
func TestFinishedJobsAreForgotten(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.KeepFinished = 20 * time.Millisecond })
	bundle := loadBundle(t, srv, "minimal")
	run := submit(t, srv, moil.Job{ID: "j1", Bundle: bundle, Eligible: moil.AnyMachine})
	run.Cancel()
	wait(t, run)
	eventually(t, "the job to be forgotten", func() bool { _, ok := srv.Run("j1"); return !ok })
	if again := submit(t, srv, moil.Job{ID: "j1", Bundle: bundle, Eligible: moil.AnyMachine}); again == run {
		t.Fatal("a forgotten job's ID still returns the old run")
	}
}

func TestSubmitRejectsJobsMachinesCouldNotRun(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	other := vectorBundle(t, "crlf-unicode")
	ok := moil.Job{Bundle: bundle, Eligible: moil.AnyMachine}
	for _, c := range []struct {
		name string
		edit func(*moil.Job)
		want string
	}{
		{"no eligibility policy", func(j *moil.Job) { j.Eligible = nil }, "Eligible is required"},
		{"no bundle", func(j *moil.Job) { j.Bundle = nil }, "Bundle is required"},
		{"bundle never added", func(j *moil.Job) { j.Bundle = other }, "AddBundle"},
		{"bad job ID", func(j *moil.Job) { j.ID = "../etc" }, "job ID"},
		{"unsafe input name", func(j *moil.Job) { j.Inputs = map[string]moil.Download{"../a": {URL: "https://e.com/a"}} }, "input name"},
		{"relative output URL", func(j *moil.Job) { j.Outputs = map[string]moil.Upload{"a.txt": {URL: "/upload"}} }, "has no host"},
		{"plain http to storage", func(j *moil.Job) { j.Inputs = map[string]moil.Download{"a": {URL: "http://s3.example.com/a"}} }, "uses http to s3.example.com"},
		{"a scheme machines don't speak", func(j *moil.Job) { j.Outputs = map[string]moil.Upload{"a": {URL: "ftp://example.com/a"}} }, "uses ftp"},
		{"unsupported method", func(j *moil.Job) {
			j.Outputs = map[string]moil.Upload{"a.txt": {URL: "https://e.com", Method: "PATCH"}}
		}, "PUT or POST"},
		{"long title", func(j *moil.Job) { j.Title = strings.Repeat("é", 201) }, "200 characters"},
		{"params that aren't JSON", func(j *moil.Job) { j.Params = make(chan int) }, "Params"},
		{"params over a control message", func(j *moil.Job) { j.Params = strings.Repeat("x", 1<<20) }, "1 MiB"},
		{"params nested too deep", func(j *moil.Job) {
			j.Params = map[string]any{"a": json.RawMessage(strings.Repeat("[", 64) + strings.Repeat("]", 64))}
		}, "nested more than 64 levels deep"},
		{"params with a number beyond a double", func(j *moil.Job) { j.Params = map[string]any{"n": new(big.Int).Lsh(big.NewInt(1), 1024)} }, "beyond the range of a double"},
		{"negative timeout", func(j *moil.Job) { j.Timeout = -time.Second }, "Timeout"},
	} {
		t.Run(c.name, func(t *testing.T) {
			job := ok
			c.edit(&job)
			_, err := srv.Submit(context.Background(), job)
			if err == nil || !strings.Contains(err.Error(), c.want) {
				t.Fatalf("err = %v, want one mentioning %q", err, c.want)
			}
		})
	}
	if _, err := srv.Submit(context.Background(), ok); err != nil {
		t.Fatalf("the valid job was rejected: %v", err)
	}
	ok.Params = map[string]any{"a": json.RawMessage(strings.Repeat("[", 63) + strings.Repeat("]", 63)), "id": uint64(1<<64 - 1)}
	if _, err := srv.Submit(context.Background(), ok); err != nil {
		t.Fatalf("params 64 levels deep were rejected: %v", err)
	}
}

// The lease defaults to two minutes, so that a machine can notice a dead
// channel and reconnect in time, and a time limit under a millisecond
// isn't sent as none.
func TestLeasesAndTimeLimitsReachTheMachine(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.LeaseTTL = 0 })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	for _, c := range []struct{ timeout, sent time.Duration }{
		{0, 0},
		{500 * time.Microsecond, time.Millisecond},
		{1500 * time.Microsecond, 2 * time.Millisecond},
		{time.Hour, time.Hour},
	} {
		run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Timeout: c.timeout})
		a := m.NextAttempt()
		if a.Lease != 2*time.Minute || a.Timeout != c.sent {
			t.Errorf("Timeout %v: the machine got a lease of %v and a time limit of %v, want 2m0s and %v", c.timeout, a.Lease, a.Timeout, c.sent)
		}
		a.Succeed(nil)
		succeeded(t, run)
	}
}

func TestConfigIsChecked(t *testing.T) {
	valid := moil.Config{Name: "klisi", VerificationURL: "https://klisi.example.com/pair", Store: moil.NewMemoryStore()}
	for _, c := range []struct {
		name string
		edit func(*moil.Config)
		want string
	}{
		{"no name", func(c *moil.Config) { c.Name = "" }, "Name"},
		{"relative verification URL", func(c *moil.Config) { c.VerificationURL = "/pair" }, "VerificationURL"},
		{"no store", func(c *moil.Config) { c.Store = nil }, "Store"},
		{"negative duration", func(c *moil.Config) { c.BidWindow = -time.Second }, "negative"},
		{"pings too rare for machines", func(c *moil.Config) { c.PingInterval = 21 * time.Second }, "PingInterval"},
	} {
		t.Run(c.name, func(t *testing.T) {
			cfg := valid
			c.edit(&cfg)
			if srv, err := moil.NewServer(cfg); err == nil || !strings.Contains(err.Error(), c.want) {
				if srv != nil {
					srv.Close()
				}
				t.Fatalf("err = %v, want one mentioning %s", err, c.want)
			}
		})
	}
	valid.PingInterval = 20 * time.Second
	srv, err := moil.NewServer(valid)
	if err != nil {
		t.Fatal(err)
	}
	srv.Close()
}

// Submit accepts the URLs machines accept: https, and http to loopback
// hosts, which machines accept from a service on loopback. Plain http
// elsewhere needs the service's explicit opt-in, as it needs the machine
// owner's.
func TestSubmitAcceptsURLsMachinesAccept(t *testing.T) {
	strict := newServer(t)
	insecure := newServer(t, func(c *moil.Config) { c.AllowInsecureHTTP = true })
	for _, srv := range []*moil.Server{strict, insecure} {
		bundle := loadBundle(t, srv, "minimal")
		for _, u := range []string{"https://s3.example.com/a?X-Amz-Signature=x", "http://127.0.0.1:9000/a", "http://localhost/a", "http://[::1]:9000/a"} {
			if _, err := srv.Submit(context.Background(), moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Inputs: map[string]moil.Download{"a": {URL: u}}}); err != nil {
				t.Errorf("%s was refused: %v", u, err)
			}
		}
	}
	bundle := loadBundle(t, insecure, "minimal")
	if _, err := insecure.Submit(context.Background(), moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Outputs: map[string]moil.Upload{"a": {URL: "http://minio:9000/a"}}}); err != nil {
		t.Errorf("AllowInsecureHTTP didn't allow http: %v", err)
	}
	if _, err := insecure.Submit(context.Background(), moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Outputs: map[string]moil.Upload{"a": {URL: "file:///etc/passwd"}}}); err == nil {
		t.Error("AllowInsecureHTTP allowed a file URL")
	}
}

// moiltest's machines refuse what real machines refuse: http beyond
// loopback, unless their owner allowed it, also after a redirect.
func TestMoiltestHoldsTransfersToTheMachinesURLPolicy(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.AllowInsecureHTTP = true })
	bundle := loadBundle(t, srv, "minimal")
	storage := newFileServer(t, map[string][]byte{"/in": []byte("audio")})
	// The same storage, at an address that isn't a loopback host by the
	// spec's definition, though it leads to one.
	elsewhere := strings.Replace(storage.URL, "127.0.0.1", "[::ffff:127.0.0.1]", 1) + "/in"
	redirect := httptestServer(t, http.RedirectHandler(elsewhere, http.StatusFound))
	job := moil.Job{Bundle: bundle, Eligible: moil.OwnedBy("alice"), Inputs: map[string]moil.Download{
		"local.ogg":      {URL: storage.URL + "/in"},
		"remote.ogg":     {URL: elsewhere},
		"redirected.ogg": {URL: redirect + "/in"},
	}}

	tb := &failureRecorder{TB: t}
	strict := moiltest.Pair(tb, srv, "alice")
	strict.Approve(bundle)
	strict.Connect()
	run := submit(t, srv, job)
	a := strict.NextAttempt()
	if got := string(a.Input("local.ogg")); got != "audio" {
		t.Fatalf("loopback input %q", got)
	}
	if msg := tb.failure(func() { a.Input("remote.ogg") }); !strings.Contains(msg, "uses http to ::ffff:127.0.0.1; only https is allowed") {
		t.Errorf("fetching an http input beyond loopback: %q", msg)
	}
	if msg := tb.failure(func() { a.Input("redirected.ogg") }); !strings.Contains(msg, "redirects to "+elsewhere+", which uses http") {
		t.Errorf("following a redirect to http beyond loopback: %q", msg)
	}
	a.Succeed(nil)
	succeeded(t, run)
	strict.Disconnect()

	lenient := moiltest.Pair(t, srv, "alice", moiltest.WithInsecureHTTP())
	lenient.Approve(bundle)
	lenient.Connect()
	submit(t, srv, job)
	b := lenient.NextAttempt()
	for _, name := range []string{"remote.ogg", "redirected.ogg"} {
		if got := string(b.Input(name)); got != "audio" {
			t.Errorf("%s: %q", name, got)
		}
	}
}

// The event stream replays from the start for every reader, ends when the
// run ends, and a reader that stalls doesn't hold up the job.
func TestEventStreamReplaysAndNeverBlocksTheServer(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})

	// A reader that takes the first event and then stalls.
	stalled, release := make(chan moil.Event, 1), make(chan struct{})
	var stalledSaw []moil.EventKind
	readerDone := make(chan struct{})
	go func() {
		defer close(readerDone)
		for ev := range run.Events(context.Background()) {
			stalledSaw = append(stalledSaw, ev.Kind)
			if len(stalledSaw) == 1 {
				stalled <- ev
				<-release
			}
		}
	}()
	<-stalled

	a := m.NextAttempt()
	for i := range 50 {
		a.Log("info", "line "+string(rune('a'+i%26)))
	}
	a.Succeed(nil)
	succeeded(t, run) // the job ended while a reader was stuck

	close(release)
	<-readerDone
	late := kinds(events(t, run))
	if len(late) != 52 || !slices.Equal(stalledSaw, late) {
		t.Fatalf("stalled reader saw %d events, late reader %d; both must see all 52 in order", len(stalledSaw), len(late))
	}

	// Stopping early and cancelling both end the iteration.
	n := 0
	for range run.Events(context.Background()) {
		if n++; n == 3 {
			break
		}
	}
	queued := submit(t, srv, moil.Job{Bundle: bundle, Eligible: func(moil.Machine) bool { return false }})
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	got := 0
	for range queued.Events(ctx) {
		got++
		cancel() // the job never runs; only the context ends the stream
	}
	if got != 1 {
		t.Fatalf("got %d events from a queued job, want its queued event", got)
	}
}

// Prepare makes fresh files for every attempt, as the attempt is
// assigned, so pre-signed URLs don't expire while a job waits or between
// attempts.
func TestPrepareMakesFilesPerAttempt(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	storage := newFileServer(t, map[string][]byte{"/in": []byte("audio")})
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	var calls []moil.Assignment
	run := submit(t, srv, moil.Job{
		Bundle:   bundle,
		Eligible: moil.OwnedBy("alice"),
		Prepare: func(ctx context.Context, a moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
			calls = append(calls, a) // one attempt at a time: no lock needed
			sig := fmt.Sprintf("?sig=%d", a.Attempt)
			return map[string]moil.Download{"in.ogg": {URL: storage.URL + "/in" + sig}},
				map[string]moil.Upload{"out.txt": {URL: storage.URL + "/out" + sig}}, nil
		},
	})
	first := m.NextAttempt()
	if first.Inputs["in.ogg"].URL != storage.URL+"/in?sig=1" || first.Outputs["out.txt"].URL != storage.URL+"/out?sig=1" {
		t.Fatalf("attempt 1 files %+v %+v", first.Inputs, first.Outputs)
	}
	first.Fail(moil.CodeOutputUpload, "the URL expired")
	second := m.NextAttempt()
	if second.Outputs["out.txt"].URL != storage.URL+"/out?sig=2" || second.Outputs["out.txt"].Method != "PUT" {
		t.Fatalf("attempt 2 files %+v", second.Outputs)
	}
	second.Output("out.txt", second.Input("in.ogg"))
	second.Succeed(nil)
	succeeded(t, run)
	if len(calls) != 2 || calls[0].JobID != run.ID() || calls[1].Attempt != 2 || calls[1].Machine.ID != m.ID() || calls[1].Machine.Owner != "alice" {
		t.Fatalf("Prepare calls %+v", calls)
	}
}

func TestPrepareErrorsEndTheJob(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	errNoCredentials := errors.New("no S3 credentials")
	for _, prepare := range []func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error){
		func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
			return nil, nil, errNoCredentials
		},
		func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
			return map[string]moil.Download{"../x": {URL: "https://e.com"}}, nil, nil
		},
		func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
			panic("bug")
		},
	} {
		run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: prepare})
		if _, err := wait(t, run); err == nil || run.State() != moil.Failed {
			t.Fatalf("err %v, state %s", err, run.State())
		}
	}
	m.Sync()
	for _, o := range m.Offers() {
		if o.Answer != moiltest.Bid {
			t.Fatalf("offers %+v", m.Offers())
		}
	}
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
		return nil, nil, errNoCredentials
	}})
	if _, err := wait(t, run); !errors.Is(err, errNoCredentials) {
		t.Fatalf("Wait = %v, want the error from Prepare", err)
	}
	if _, err := srv.Submit(context.Background(), moil.Job{Bundle: bundle, Eligible: moil.AnyMachine,
		Inputs: map[string]moil.Download{"a": {URL: "https://e.com"}},
		Prepare: func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
			return nil, nil, nil
		},
	}); err == nil {
		t.Fatal("a job with both Inputs and Prepare was accepted")
	}
}

// While Prepare runs, the machine is held for the job; if the job is
// cancelled, Prepare's context ends, and if the machine leaves, the job is
// offered again.
func TestPrepareIsInterruptible(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()

	started, stopped := make(chan struct{}), make(chan struct{})
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: func(ctx context.Context, a moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
		close(started)
		<-ctx.Done()
		close(stopped)
		return nil, nil, ctx.Err()
	}})
	<-started
	other := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine})
	m.Sync()
	if n := len(m.Offers()); n != 1 {
		t.Fatalf("a held machine was offered another job: %d offers", n)
	}
	run.Cancel()
	if _, err := wait(t, run); !errors.Is(err, moil.ErrCancelled) {
		t.Fatalf("err %v", err)
	}
	<-stopped
	m.NextAttempt().Succeed(nil) // the machine is free again
	succeeded(t, other)

	release := make(chan struct{})
	t.Cleanup(func() {
		select {
		case <-release:
		default:
			close(release)
		}
	})
	moved := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: func(ctx context.Context, a moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
		if a.Machine.ID == m.ID() {
			<-release
		}
		return nil, nil, nil
	}})
	m.NextOffer()
	m.NextOffer()
	if o := m.NextOffer(); o.JobID != moved.ID() || o.Answer != moiltest.Bid {
		t.Fatalf("offer %+v", o)
	}
	m.Disconnect()
	eventually(t, "the machine to go offline", func() bool { return !machine(t, srv, m.ID()).Online() })
	close(release)
	replacement := moiltest.Pair(t, srv, "alice")
	replacement.Approve(bundle)
	replacement.Connect()
	a := replacement.NextAttempt()
	a.Succeed(nil)
	if res := succeeded(t, moved); res.Machine != replacement.ID() || res.Attempt != 2 {
		t.Fatalf("result from %s attempt %d", res.Machine, res.Attempt)
	}
}
