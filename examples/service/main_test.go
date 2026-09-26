package main

import (
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"strings"
	"testing"

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
	for _, want := range []string{"Ada&#39;s MacBook", code, "Confirm"} {
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
