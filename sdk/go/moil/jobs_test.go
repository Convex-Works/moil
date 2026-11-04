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
	"sync/atomic"
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

// Submitting a job while one with its ID is unfinished returns the same
// Run, so a service can resubmit by ID without doubling work. Once the job
// has finished, the ID runs again, as a retry button would want.
func TestResubmittingByID(t *testing.T) {
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
	if again := submit(t, srv, job); again != first {
		t.Fatal("resubmitting a running job made a second run")
	}
	a.ScriptError("the recording is empty", false)
	if _, err := wait(t, first); err == nil {
		t.Fatal("the job succeeded")
	}

	retry := submit(t, srv, job)
	if retry == first || retry.ID() != first.ID() {
		t.Fatal("resubmitting a finished job didn't run it again")
	}
	if got, _ := srv.Run(job.ID); got != retry {
		t.Fatal("Run doesn't find the latest run")
	}
	b := m.NextAttempt()
	if b.Title != "a different definition" || b.Number != 2 {
		t.Errorf("the rerun is attempt %d titled %q; attempt numbers carry on, and the new definition counts", b.Number, b.Title)
	}
	b.Succeed(nil)
	if res := succeeded(t, retry); res.Attempt != 2 {
		t.Errorf("result from attempt %d", res.Attempt)
	}
	if _, err := wait(t, first); err == nil {
		t.Error("the rerun changed how the first run ended")
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
	valid := moil.Config{Name: "tide", VerificationURL: "https://tide.example.com/pair", Store: moil.NewMemoryStore()}
	for _, c := range []struct {
		name string
		edit func(*moil.Config)
		want string
	}{
		{"no name", func(c *moil.Config) { c.Name = "" }, "Name"},
		{"relative verification URL", func(c *moil.Config) { c.VerificationURL = "/pair" }, "VerificationURL"},
		{"verification URL off the web", func(c *moil.Config) { c.VerificationURL = "ftp://tide.example.com/pair" }, "VerificationURL"},
		{"verification URL with a user name", func(c *moil.Config) { c.VerificationURL = "https://tide.example.com@evil.example/pair" }, "VerificationURL"},
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

// While Prepare runs, the machine is held for the job. If the job is
// cancelled, Prepare's context ends.
func TestPrepareHoldsTheMachine(t *testing.T) {
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
}

// A machine that leaves, or is removed, while Prepare makes its attempt's
// files lets go of the job at once: Prepare's context ends, and the job
// goes to another machine.
func TestPrepareEndsWhenItsMachineLeaves(t *testing.T) {
	for _, leave := range []struct {
		name string
		do   func(*moil.Server, *moiltest.Machine) error
	}{
		{"disconnected", func(_ *moil.Server, m *moiltest.Machine) error { m.Disconnect(); return nil }},
		{"removed", func(srv *moil.Server, m *moiltest.Machine) error {
			return srv.RemoveMachine(context.Background(), m.ID())
		}},
	} {
		t.Run(leave.name, func(t *testing.T) {
			srv := newServer(t)
			bundle := loadBundle(t, srv, "minimal")
			first := moiltest.Pair(t, srv, "alice")
			first.Approve(bundle)
			first.Connect()
			preparing, stopped := make(chan struct{}), make(chan error, 1)
			run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: func(ctx context.Context, a moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
				if a.Machine.ID == first.ID() {
					close(preparing)
					<-ctx.Done() // held until the machine leaves
					stopped <- ctx.Err()
					return nil, nil, ctx.Err()
				}
				return nil, nil, nil
			}})
			<-preparing
			if err := leave.do(srv, first); err != nil {
				t.Fatal(err)
			}
			select {
			case err := <-stopped:
				if !errors.Is(err, context.Canceled) {
					t.Fatalf("Prepare's context ended with %v", err)
				}
			case <-time.After(10 * time.Second):
				t.Fatal("Prepare's context didn't end when its machine left")
			}
			second := moiltest.Pair(t, srv, "alice")
			second.Approve(bundle)
			second.Connect()
			second.NextAttempt().Succeed(nil)
			if res := succeeded(t, run); res.Machine != second.ID() || res.Attempt != 2 {
				t.Fatalf("result from %s attempt %d", res.Machine, res.Attempt)
			}
		})
	}
}

// A heldCall is one call of a Job.Prepare made by heldPrepare, waiting for
// the test to release it.
type heldCall struct {
	moil.Assignment
	release chan struct{}
}

// heldPrepare returns a Job.Prepare that hands each call to the test on
// calls, then waits until the test closes its release channel or ctx ends.
func heldPrepare(calls chan<- heldCall) func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
	return func(ctx context.Context, a moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
		c := heldCall{Assignment: a, release: make(chan struct{})}
		select {
		case calls <- c:
		case <-ctx.Done():
			return nil, nil, ctx.Err()
		}
		select {
		case <-c.release:
			return nil, nil, nil
		case <-ctx.Done():
			return nil, nil, ctx.Err()
		}
	}
}

// nextCall waits for the next call of a heldPrepare, and returns false if
// the run ended instead.
func nextCall(t *testing.T, calls <-chan heldCall, run *moil.Run) (heldCall, bool) {
	t.Helper()
	select {
	case c := <-calls:
		return c, true
	case <-run.Done():
		return heldCall{}, false
	case <-time.After(10 * time.Second):
		t.Fatalf("waited 10s for Prepare or the end of run %s", run.ID())
		return heldCall{}, false
	}
}

// givenUp waits until the run has retried n attempts, or ended.
func givenUp(t *testing.T, run *moil.Run, n int) {
	t.Helper()
	eventually(t, fmt.Sprintf("attempt %d to be given up", n), func() bool {
		select {
		case <-run.Done():
			return true
		default:
			return len(retries(eventsSoFar(run))) >= n
		}
	})
}

// A backOut is a way a machine that bid can back out of the attempt while
// Job.Prepare makes it, with the refusal the service charges it with.
type backOut struct {
	name string
	code moil.ErrorCode
}

var backOuts = []backOut{
	{"paused", moil.CodeBusy},
	{"busy elsewhere", moil.CodeBusy},
	{"approval withdrawn", moil.CodeNotApproved},
	{"disconnected", moil.CodeBusy},
}

// out backs the machine out, and returns once the service has seen it.
func (b backOut) out(t *testing.T, srv *moil.Server, m *moiltest.Machine, bundle *moil.Bundle) {
	t.Helper()
	switch b.name {
	case "paused":
		m.SetState(moil.Paused)
	case "busy elsewhere":
		m.SetState(moil.Busy)
	case "approval withdrawn":
		m.Revoke(bundle)
	case "disconnected":
		m.Disconnect()
		eventually(t, "the service to see the machine leave", func() bool { return !machine(t, srv, m.ID()).Online() })
		return
	}
	m.Sync()
}

// in brings the machine back, ready for the job again.
func (b backOut) in(m *moiltest.Machine, bundle *moil.Bundle) {
	switch b.name {
	case "approval withdrawn":
		m.Approve(bundle)
	case "disconnected":
		m.Connect()
	default:
		m.SetState(moil.Idle)
	}
}

// A machine that backs out once while Prepare makes its attempt costs the
// job nothing: the attempt never reached it, so like one it refused, it
// doesn't count toward MaxAttempts, and the job runs when the machine is
// back.
func TestBackingOutDuringPrepareOnceCostsNothing(t *testing.T) {
	for _, back := range backOuts {
		t.Run(back.name, func(t *testing.T) {
			srv := newServer(t, func(c *moil.Config) { c.MaxAttempts = 1 })
			bundle := loadBundle(t, srv, "minimal")
			m := moiltest.Pair(t, srv, "alice")
			m.Approve(bundle)
			m.Connect()
			calls := make(chan heldCall)
			run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: heldPrepare(calls)})

			first, ok := nextCall(t, calls, run)
			if !ok {
				t.Fatal("the job ended before Prepare ran")
			}
			back.out(t, srv, m, bundle)
			close(first.release)
			givenUp(t, run, 1)
			back.in(m, bundle)

			second, ok := nextCall(t, calls, run)
			if !ok {
				_, err := wait(t, run)
				t.Fatalf("the job ended after the machine backed out once: %v", err)
			}
			if second.Attempt != 2 || second.Machine.ID != m.ID() {
				t.Fatalf("prepared attempt %d for %s", second.Attempt, second.Machine.ID)
			}
			close(second.release)
			a := m.NextAttempt()
			if a.Number != 2 {
				t.Fatalf("attempt %d assigned, want 2", a.Number)
			}
			a.Succeed(nil)
			if res := succeeded(t, run); res.Attempt != 2 {
				t.Fatalf("result from attempt %d", res.Attempt)
			}
			evs := events(t, run)
			if got := retries(evs); !slices.Equal(got, []moil.ErrorCode{back.code}) {
				t.Fatalf("retries %v, want [%s]", got, back.code)
			}
			for _, ev := range evs {
				if ev.Kind == moil.EventRetrying && (ev.Attempt != 1 || ev.Machine != m.ID() || ev.Err.Attempt != 1 || ev.Err.Machine != m.ID()) {
					t.Fatalf("retrying event %+v, error %+v; want attempt 1 on %s", ev, ev.Err, m.ID())
				}
			}
		})
	}
}

// A machine that keeps bidding and then backing out while Prepare makes
// its attempt can't make Prepare run forever: the job fails after ten such
// attempts, as it does after ten refusals, with the last one's error.
func TestMachinesCantBackOutDuringPrepareForever(t *testing.T) {
	for _, back := range backOuts {
		t.Run(back.name, func(t *testing.T) {
			srv := newServer(t, func(c *moil.Config) { c.MaxAttempts = 1 })
			bundle := loadBundle(t, srv, "minimal")
			m := moiltest.Pair(t, srv, "alice")
			m.Approve(bundle)
			m.Connect()
			calls := make(chan heldCall)
			run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: heldPrepare(calls)})
			prepared := 0
			for {
				c, ok := nextCall(t, calls, run)
				if !ok {
					break
				}
				prepared++
				if prepared > 20 {
					t.Fatal("Prepare ran 20 times for a machine that backs out of every attempt")
				}
				if c.Attempt != prepared {
					t.Fatalf("prepared attempt %d, want %d", c.Attempt, prepared)
				}
				back.out(t, srv, m, bundle)
				close(c.release)
				givenUp(t, run, prepared)
				back.in(m, bundle)
			}
			if prepared != 10 {
				t.Fatalf("Prepare ran %d times, want 10", prepared)
			}
			_, err := wait(t, run)
			var jerr *moil.JobError
			if !errors.As(err, &jerr) || jerr.Code != back.code || !jerr.Retryable || jerr.Attempt != 10 || jerr.Machine != m.ID() || run.State() != moil.Failed {
				t.Fatalf("err = %#v, state %s; want attempt 10's %s", err, run.State(), back.code)
			}
			if n := len(retries(events(t, run))); n != 9 {
				t.Fatalf("%d attempts were retried, want 9", n)
			}
		})
	}
}

// Prepare can ask for another try, as when storage is briefly down; the
// job goes back to the queue, and the tries count toward MaxAttempts.
func TestPrepareCanRetry(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.MaxAttempts = 3 })
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	unavailable := &moil.JobError{Code: "storage_unavailable", Message: "S3 is down", Retryable: true}
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: func(ctx context.Context, a moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
		if a.Attempt == 1 {
			return nil, nil, unavailable
		}
		return nil, nil, nil
	}})
	a := m.NextAttempt()
	if a.Number != 2 {
		t.Fatalf("attempt %d", a.Number)
	}
	a.Succeed(nil)
	succeeded(t, run)
	var retried []moil.JobError
	for _, ev := range events(t, run) {
		if ev.Kind == moil.EventRetrying {
			retried = append(retried, *ev.Err)
		}
	}
	if len(retried) != 1 || retried[0].Code != "storage_unavailable" || retried[0].Attempt != 1 || retried[0].Machine != m.ID() {
		t.Fatalf("retries %+v", retried)
	}
	if unavailable.Attempt != 0 {
		t.Error("the Server changed Prepare's error")
	}

	always := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: func(context.Context, moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
		return nil, nil, unavailable
	}})
	_, err := wait(t, always)
	var jerr *moil.JobError
	if !errors.As(err, &jerr) || jerr.Code != "storage_unavailable" || jerr.Attempt != 3 {
		t.Fatalf("a Prepare that always asks to retry ended the job with %v, want its error on attempt 3", err)
	}
}

// Close ends Prepare's context and waits for it to return.
func TestCloseWaitsForPrepare(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	started := make(chan struct{})
	var returned atomic.Bool
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Prepare: func(ctx context.Context, a moil.Assignment) (map[string]moil.Download, map[string]moil.Upload, error) {
		close(started)
		<-ctx.Done()
		time.Sleep(20 * time.Millisecond) // cleaning up, slowly
		returned.Store(true)
		return nil, nil, ctx.Err()
	}})
	<-started
	srv.Close()
	if !returned.Load() {
		t.Fatal("Close returned before Prepare did")
	}
	if _, err := wait(t, run); !errors.Is(err, moil.ErrClosed) {
		t.Fatalf("Wait = %v, want ErrClosed", err)
	}
}

// A result only ever names the job's declared outputs, whatever a machine
// reports.
func TestResultsNameOnlyDeclaredOutputs(t *testing.T) {
	srv := newServer(t)
	bundle := loadBundle(t, srv, "minimal")
	m := moiltest.Pair(t, srv, "alice")
	m.Approve(bundle)
	m.Connect()
	run := submit(t, srv, moil.Job{Bundle: bundle, Eligible: moil.AnyMachine, Outputs: map[string]moil.Upload{"out.txt": {URL: "https://s3.example.com/out"}}})
	a := m.NextAttempt()
	file := map[string]any{"size_bytes": 5, "sha256": strings.Repeat("a", 64)}
	m.Send(map[string]any{"type": "done", "job_id": a.JobID, "attempt": a.Number, "seq": 1, "outcome": "succeeded",
		"result": map[string]any{"files": map[string]any{"out.txt": file, "passwords.txt": file}}})
	res := succeeded(t, run)
	if len(res.Files) != 1 || res.Files["out.txt"].SizeBytes != 5 {
		t.Fatalf("result files %+v, want only out.txt", res.Files)
	}
}
