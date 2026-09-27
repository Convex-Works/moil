package main

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

// The demo works end to end with a machine: pairing through its
// confirmation page, then a job whose input it serves and whose output it
// receives, followed live.
func TestDemo(t *testing.T) {
	var handler http.Handler
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { handler.ServeHTTP(w, r) }))
	defer ts.Close()
	srv, err := moil.NewServer(moil.Config{Name: "moil demo", VerificationURL: ts.URL + "/pair", Store: moil.NewMemoryStore()})
	if err != nil {
		t.Fatal(err)
	}
	defer srv.Close()
	bundle, err := moil.LoadBundle(os.DirFS("../../spec/vectors/bundles/minimal"))
	if err != nil {
		t.Fatal(err)
	}
	srv.AddBundle(bundle)
	handler = newDemo(srv, ts.URL).routes()

	m := moiltest.New(t, ts.URL+"/moil", moiltest.WithName("Ada's MacBook"))
	code := m.StartPairing()
	page := get(t, ts.URL+"/pair?code="+url.QueryEscape(code))
	for _, want := range []string{"Ada&#39;s MacBook", code, "<b>" + ts.URL + "/moil</b>", "Confirm", "Only confirm a code your own moil app shows you."} {
		if !strings.Contains(page, want) {
			t.Fatalf("the confirmation page lacks %q:\n%s", want, page)
		}
	}
	home := postForm(t, ts.URL+"/pair", url.Values{"code": {code}, "action": {"confirm"}})
	if !strings.Contains(home, "is paired") {
		t.Fatalf("after confirming:\n%s", home)
	}
	m.FinishPairing()
	m.Approve(bundle)
	m.Connect()
	if home := get(t, ts.URL+"/"); !strings.Contains(home, `<span class="pill idle">idle</span>`) || !strings.Contains(home, "✓") {
		t.Fatalf("the home page doesn't show the machine idle with the bundle approved:\n%s", home)
	}

	jobPage := postForm(t, ts.URL+"/jobs", url.Values{
		"bundle": {bundle.Hash()}, "title": {"Shout"}, "text": {"hello"}, "params": {`{"loud": true}`}, "outputs": {"output.txt"},
	})
	if !strings.Contains(jobPage, "Shout") {
		t.Fatalf("job page:\n%s", jobPage)
	}
	a := m.NextAttempt()
	if string(a.Params) != `{"loud":true}` {
		t.Fatalf("params %s", a.Params)
	}
	text := a.Input("text.txt")
	a.Progress(0.5, "shouting")
	a.Output("output.txt", []byte(strings.ToUpper(string(text))))
	a.Succeed(map[string]int{"chars": len(text)})

	events := get(t, ts.URL+"/jobs/"+a.JobID+"/events")
	for _, want := range []string{"attempt 1 assigned to " + m.ID(), "50% shouting", "event: end"} {
		if !strings.Contains(events, want) {
			t.Fatalf("the event stream lacks %q:\n%s", want, events)
		}
	}
	result := get(t, ts.URL+"/jobs/"+a.JobID)
	start := strings.Index(result, ts.URL+"/files/")
	if start < 0 || !strings.Contains(result, `&#34;chars&#34;: 5`) {
		t.Fatalf("the result page lacks the output link or the metadata:\n%s", result)
	}
	link := result[start : start+strings.IndexByte(result[start:], '"')]
	if got := get(t, link); got != "HELLO" {
		t.Fatalf("the output the machine uploaded reads %q", got)
	}
}

func get(t *testing.T, url string) string {
	t.Helper()
	resp, err := http.Get(url)
	if err != nil {
		t.Fatal(err)
	}
	return body(t, resp)
}

func postForm(t *testing.T, url string, form url.Values) string {
	t.Helper()
	resp, err := http.PostForm(url, form)
	if err != nil {
		t.Fatal(err)
	}
	return body(t, resp)
}

func body(t *testing.T, resp *http.Response) string {
	t.Helper()
	defer resp.Body.Close()
	data, err := io.ReadAll(resp.Body)
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("%s: %d %s %v", resp.Request.URL, resp.StatusCode, data, err)
	}
	return string(data)
}

// newTestDemo serves the demo for a test, with the minimal bundle
// published.
func newTestDemo(t *testing.T) (*demo, *httptest.Server, *moil.Bundle) {
	t.Helper()
	var handler http.Handler
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { handler.ServeHTTP(w, r) }))
	t.Cleanup(ts.Close)
	srv, err := moil.NewServer(moil.Config{Name: "moil demo", VerificationURL: ts.URL + "/pair", Store: moil.NewMemoryStore()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { srv.Close() })
	bundle, err := moil.LoadBundle(os.DirFS("../../spec/vectors/bundles/minimal"))
	if err != nil {
		t.Fatal(err)
	}
	srv.AddBundle(bundle)
	d := newDemo(srv, ts.URL)
	handler = d.routes()
	return d, ts, bundle
}

// Another site can't use a signed-in user's browser to confirm a pairing,
// which would hand an attacker's machine the user's jobs, or to submit or
// remove anything.
func TestCrossSiteFormsAreRefused(t *testing.T) {
	d, ts, bundle := newTestDemo(t)
	m := moiltest.New(t, ts.URL+"/moil")
	code := m.StartPairing()
	for _, c := range []struct {
		path   string
		form   url.Values
		header http.Header
	}{
		{"/pair", url.Values{"code": {code}, "action": {"confirm"}}, http.Header{"Origin": {"https://evil.example"}}},
		{"/pair", url.Values{"code": {code}, "action": {"confirm"}}, http.Header{"Sec-Fetch-Site": {"cross-site"}}},
		{"/pair", url.Values{"code": {code}, "action": {"deny"}}, http.Header{"Sec-Fetch-Site": {"same-site"}}},
		{"/jobs", url.Values{"bundle": {bundle.Hash()}, "text": {"hi"}}, http.Header{"Origin": {"null"}}},
		{"/machines/m_1/remove", nil, http.Header{"Sec-Fetch-Site": {"cross-site"}}},
	} {
		req, _ := http.NewRequest(http.MethodPost, ts.URL+c.path, strings.NewReader(c.form.Encode()))
		req.Header = c.header
		req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != http.StatusForbidden {
			t.Errorf("POST %s with %v: %d, want 403", c.path, c.header, resp.StatusCode)
		}
	}
	if _, ok := d.srv.PendingPairing(code); !ok {
		t.Fatal("a cross-site request settled the pairing")
	}
	d.mu.Lock()
	jobs := len(d.order)
	d.mu.Unlock()
	if jobs != 0 {
		t.Fatal("a cross-site request submitted a job")
	}

	// The service's own pages still work, as browsers send them.
	req, _ := http.NewRequest(http.MethodPost, ts.URL+"/pair", strings.NewReader(url.Values{"code": {code}, "action": {"confirm"}}.Encode()))
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("Origin", ts.URL)
	req.Header.Set("Sec-Fetch-Site", "same-origin")
	if page := body(t, do(t, req)); !strings.Contains(page, "is paired") {
		t.Fatalf("confirming from the service's own page:\n%s", page)
	}
}

// Outputs hold whatever the machine uploaded, so they're served as
// downloads: never as a page of the service that could run scripts.
func TestOutputsAreServedAsDownloads(t *testing.T) {
	d, ts, _ := newTestDemo(t)
	token := d.addFile(&file{name: "report.html"})
	req, _ := http.NewRequest(http.MethodPut, d.fileURL(token), strings.NewReader("<script>steal(document.cookie)</script>"))
	req.Header.Set("Content-Type", "text/html")
	do(t, req).Body.Close()
	resp, err := http.Get(ts.URL + "/files/" + token)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	h := resp.Header
	if h.Get("Content-Type") != "application/octet-stream" || h.Get("X-Content-Type-Options") != "nosniff" ||
		h.Get("Content-Disposition") != "attachment; filename=report.html" {
		t.Fatalf("the output is served with %v", h)
	}
}

// Machines upload outputs while users download them.
func TestFilesAreSafeToUseConcurrently(t *testing.T) {
	d, _, _ := newTestDemo(t)
	token := d.addFile(&file{name: "out.txt"})
	var wg sync.WaitGroup
	for i := range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			method := http.MethodGet
			if i%2 == 0 {
				method = http.MethodPut
			}
			for range 20 {
				req, _ := http.NewRequest(method, d.fileURL(token), strings.NewReader("output"))
				resp, err := http.DefaultClient.Do(req)
				if err != nil {
					t.Error(err)
					return
				}
				io.Copy(io.Discard, resp.Body)
				resp.Body.Close()
			}
		}()
	}
	wg.Wait()
}

// Shutting down, the demo waits for the requests in flight before run
// returns and main exits.
func TestShutdownWaitsForRequestsInFlight(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, stop := context.WithCancel(context.Background())
	defer stop()
	returned := make(chan error, 1)
	go func() {
		returned <- run(ctx, ln, "http://"+ln.Addr().String(), t.TempDir(), nil, slog.New(slog.DiscardHandler))
	}()

	// A form whose body is still arriving.
	conn, err := net.Dial("tcp", ln.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	form := "bundle=none&text=hello"
	fmt.Fprintf(conn, "POST /jobs HTTP/1.1\r\nHost: %s\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: %d\r\n\r\n%s", ln.Addr(), len(form), form[:6])
	time.Sleep(100 * time.Millisecond) // the request reaches its handler
	stop()
	select {
	case err := <-returned:
		t.Fatalf("run returned (%v) with a request in flight", err)
	case <-time.After(200 * time.Millisecond):
	}
	fmt.Fprint(conn, form[6:])
	resp, err := http.ReadResponse(bufio.NewReader(conn), nil)
	if err != nil {
		t.Fatalf("the request in flight got no answer: %v", err)
	}
	resp.Body.Close()
	select {
	case err := <-returned:
		if err != nil {
			t.Fatalf("run returned %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("run didn't return after the last request")
	}
}

func do(t *testing.T, req *http.Request) *http.Response {
	t.Helper()
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	return resp
}
