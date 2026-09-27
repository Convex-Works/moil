package e2e

import (
	"context"
	"io"
	"net"
	"net/http"
	"net/url"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"syscall"
	"testing"
	"time"
)

// The example service works with the real moil: the machine pairs through
// the service's own confirmation page, its owner approves the echo bundle
// the service publishes, and a job submitted through the service's web
// form runs on the machine and uploads its output to the service.
func TestTheExampleServiceWorksWithMoil(t *testing.T) {
	t.Parallel()
	example := startExample(t)
	m := newMachine(t, "Alice's MacBook")
	m.PairWith(example.url+"/moil", func(code string) {
		page := example.get(t, "/pair?code="+url.QueryEscape(code))
		for _, want := range []string{"Alice&#39;s MacBook", code, "Confirm"} {
			if !strings.Contains(page, want) {
				t.Fatalf("the confirmation page doesn't show %q:\n%s", want, page)
			}
		}
		if home := example.post(t, "/pair", url.Values{"code": {code}, "action": {"confirm"}}); !strings.Contains(home, "is paired") {
			t.Fatalf("confirming the pairing led to:\n%s", home)
		}
	})

	// The owner looks through the service's bundles and approves echo.
	bundles := m.moil("review", m.serviceID)
	echo := regexp.MustCompile(`(?m)^([0-9a-f]{64})\s+not approved\s+echo 1\.0\.0$`).FindStringSubmatch(bundles)
	if echo == nil {
		t.Fatalf("moil review doesn't list the echo bundle as waiting for approval:\n%s", bundles)
	}
	m.ApproveHash(echo[1])
	m.Start()

	jobPage := example.post(t, "/jobs", url.Values{
		"bundle":  {echo[1]},
		"title":   {"Shout"},
		"text":    {"hello\nworld\n"},
		"params":  {`{"shout": true}`},
		"outputs": {"echo.txt"},
	})
	id := regexp.MustCompile(`<code>([A-Za-z0-9_.-]+)</code>`).FindStringSubmatch(jobPage)
	if id == nil {
		t.Fatalf("the job page doesn't show the job's ID:\n%s", jobPage)
	}
	// The page follows the job live until it ends.
	events := example.get(t, "/jobs/"+id[1]+"/events")
	for _, want := range []string{"attempt 1 assigned to", "running", `50% line 1 of 2`, `{\"line\":2,\"text\":\"WORLD\"}`, "event: end"} {
		if !strings.Contains(events, want) {
			t.Errorf("the job's event stream lacks %q:\n%s", want, events)
		}
	}
	result := example.get(t, "/jobs/"+id[1])
	output := regexp.MustCompile(`href="(` + regexp.QuoteMeta(example.url) + `/files/[^"]+)">echo.txt<`).FindStringSubmatch(result)
	if output == nil || !strings.Contains(result, "&#34;shouted&#34;: true") {
		t.Fatalf("the result page shows neither the output nor the meta:\n%s", result)
	}
	if got := example.get(t, strings.TrimPrefix(output[1], example.url)); got != "HELLO\nWORLD\n" {
		t.Errorf("the service received the output %q", got)
	}
}

// example is examples/service, built and running.
type example struct {
	url    string
	client *http.Client
}

func startExample(t *testing.T) *example {
	t.Helper()
	log := logFor(t)
	bin := filepath.Join(t.TempDir(), "service")
	build := exec.Command("go", "build", "-o", bin, ".")
	build.Dir = filepath.Join(repoRoot, "examples", "service")
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("building the example service: %v\n%s", err, out)
	}

	addr := freeAddr(t)
	cmd := exec.Command(bin, "-addr", addr, "-bundles", filepath.Join(repoRoot, "examples", "bundles", "echo"), "-data", t.TempDir())
	cmd.Stdout, cmd.Stderr = log.writer("example"), log.writer("example")
	cmd.WaitDelay = 5 * time.Second
	inReaperGroup(cmd)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	exited := make(chan error, 1)
	go func() { exited <- cmd.Wait() }()
	t.Cleanup(func() {
		// As a service manager stops it.
		cmd.Process.Signal(syscall.SIGTERM)
		select {
		case err := <-exited:
			if err != nil {
				t.Errorf("the example service didn't shut down cleanly on SIGTERM: %v", err)
			}
		case <-time.After(patience):
			cmd.Process.Signal(syscall.SIGKILL)
			<-exited
			t.Errorf("the example service didn't exit within %v of SIGTERM", patience)
		}
	})

	e := &example{url: "http://" + addr, client: &http.Client{Timeout: patience}}
	eventually(t, "the example service to start", func() bool {
		select {
		case err := <-exited:
			t.Fatalf("the example service exited: %v", err)
		default:
		}
		resp, err := e.client.Get(e.url + "/moil/v1/info")
		if err == nil {
			resp.Body.Close()
		}
		return err == nil && resp.StatusCode == http.StatusOK
	})
	return e
}

// freeAddr returns a loopback address with a port nothing listens on.
func freeAddr(t *testing.T) string {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	return ln.Addr().String()
}

func (e *example) get(t *testing.T, path string) string {
	t.Helper()
	return e.body(t, e.do(t, http.MethodGet, path, nil))
}

func (e *example) post(t *testing.T, path string, form url.Values) string {
	t.Helper()
	return e.body(t, e.do(t, http.MethodPost, path, form))
}

func (e *example) do(t *testing.T, method, path string, form url.Values) *http.Response {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), patience)
	t.Cleanup(cancel)
	req, err := http.NewRequestWithContext(ctx, method, e.url+path, strings.NewReader(form.Encode()))
	if err != nil {
		t.Fatal(err)
	}
	if form != nil {
		req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	}
	resp, err := e.client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	return resp
}

func (e *example) body(t *testing.T, resp *http.Response) string {
	t.Helper()
	defer resp.Body.Close()
	data, err := io.ReadAll(resp.Body)
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("%s %s: %d %v\n%s", resp.Request.Method, resp.Request.URL, resp.StatusCode, err, data)
	}
	return string(data)
}
