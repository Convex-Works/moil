// Command service is a small moil service for trying the moil app end to
// end: pair a machine, approve a bundle, run a job and watch it.
//
//	go run . -addr 127.0.0.1:8787 -bundles ../bundles/echo -data .demo
//
// then add http://127.0.0.1:8787/moil as a service in the moil app.
//
// It has a single user, "demo", and no sign-in, so keep it on a loopback
// address. It is also a worked example of the SDK: pairing confirmation,
// the machine list, publishing bundles, submitting a job whose files the
// service serves and receives itself, and following the job live.
package main

import (
	"context"
	"crypto/rand"
	"embed"
	"encoding/base64"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"html/template"
	"io"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
)

// owner is the demo's only user. A real service takes it from the session.
const owner = "demo"

func main() {
	addr := flag.String("addr", "127.0.0.1:8787", "address to listen on")
	public := flag.String("url", "", "URL browsers and machines reach this service at (default http://ADDR)")
	data := flag.String("data", ".demo", "directory for the machine store, so pairings survive restarts")
	var bundleDirs []string
	flag.Func("bundles", "bundle directory to publish; repeat the flag or separate with commas", func(s string) error {
		bundleDirs = append(bundleDirs, strings.Split(s, ",")...)
		return nil
	})
	flag.Parse()
	if *public == "" {
		*public = "http://" + *addr
	}
	log := slog.New(slog.NewTextHandler(os.Stderr, nil))
	if err := run(*addr, strings.TrimSuffix(*public, "/"), *data, bundleDirs, log); err != nil {
		log.Error(err.Error())
		os.Exit(1)
	}
}

func run(addr, public, dataDir string, bundleDirs []string, log *slog.Logger) error {
	store, err := moil.NewFileStore(filepath.Join(dataDir, "machines.json"))
	if err != nil {
		return err
	}
	srv, err := moil.NewServer(moil.Config{
		Name:            "moil demo",
		VerificationURL: public + "/pair",
		Store:           store,
		Logger:          log,
	})
	if err != nil {
		return err
	}
	for _, dir := range bundleDirs {
		b, err := moil.LoadBundle(os.DirFS(dir))
		if err != nil {
			return fmt.Errorf("bundle %s: %w", dir, err)
		}
		srv.AddBundle(b)
	}

	httpServer := &http.Server{Addr: addr, Handler: newDemo(srv, public).routes(), ReadHeaderTimeout: 10 * time.Second}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	go func() {
		<-ctx.Done()
		srv.Close() // ends runs, so live job pages finish too
		httpServer.Shutdown(context.Background())
	}()
	log.Info("moil demo service ready", "open", public, "moil base URL", public+"/moil", "bundles", len(bundleDirs))
	if err := httpServer.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		return err
	}
	return nil
}

// demo is the service: the moil Server, plus the little a real service
// keeps about its own jobs.
type demo struct {
	srv    *moil.Server
	public string

	mu    sync.Mutex
	files map[string]*file // by capability token
	jobs  map[string]*job  // by job ID
	order []*job           // newest first
}

// A file is a job's input, which the service serves, or one of its
// outputs, which the machine uploads to the service. Its URL holds a
// random token, standing in for a pre-signed object-storage URL.
type file struct {
	name        string
	data        []byte
	contentType string
	received    bool
}

type job struct {
	Title   string
	Bundle  *moil.Bundle
	Created time.Time
	Outputs map[string]string // name → token
	Run     *moil.Run
}

// newDemo returns the demo service for srv, reached at the public URL.
func newDemo(srv *moil.Server, public string) *demo {
	return &demo{srv: srv, public: public, files: map[string]*file{}, jobs: map[string]*job{}}
}

func (d *demo) routes() http.Handler {
	mux := http.NewServeMux()
	mux.Handle("/moil/", http.StripPrefix("/moil", d.srv.Handler()))
	mux.HandleFunc("GET /{$}", d.home)
	mux.HandleFunc("GET /pair", d.pairPage)
	mux.HandleFunc("POST /pair", d.pair)
	mux.HandleFunc("POST /machines/{id}/remove", d.removeMachine)
	mux.HandleFunc("POST /jobs", d.submit)
	mux.HandleFunc("GET /jobs/{id}", d.jobPage)
	mux.HandleFunc("GET /jobs/{id}/events", d.jobEvents)
	mux.HandleFunc("GET /files/{token}", d.getFile)
	mux.HandleFunc("PUT /files/{token}", d.putFile)
	mux.HandleFunc("POST /files/{token}", d.putFile)
	return mux
}

// Pages.

func (d *demo) home(w http.ResponseWriter, r *http.Request) {
	machines, err := d.srv.Machines(r.Context(), owner)
	if err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	d.mu.Lock()
	jobs := slices.Clone(d.order)
	d.mu.Unlock()
	render(w, "home.html", map[string]any{
		"MoilURL":  d.public + "/moil",
		"Machines": machines,
		"Bundles":  d.srv.Bundles(),
		"Jobs":     jobs,
		"Notice":   r.URL.Query().Get("notice"),
	})
}

// pairPage is the page at Config.VerificationURL: the app opens it with
// the machine's code, and the signed-in user checks the machine is theirs.
func (d *demo) pairPage(w http.ResponseWriter, r *http.Request) {
	code := r.URL.Query().Get("code")
	p, ok := d.srv.PendingPairing(code)
	render(w, "pair.html", map[string]any{"Code": code, "Pairing": p, "Found": ok})
}

func (d *demo) pair(w http.ResponseWriter, r *http.Request) {
	code := r.FormValue("code")
	if r.FormValue("action") == "deny" {
		if err := d.srv.DenyPairing(code); err != nil {
			http.Error(w, err.Error(), http.StatusNotFound)
			return
		}
		redirect(w, r, "/", "The machine was turned away.")
		return
	}
	m, err := d.srv.ConfirmPairing(r.Context(), code, owner)
	if err != nil {
		http.Error(w, err.Error(), http.StatusNotFound)
		return
	}
	redirect(w, r, "/", m.Name+" is paired. It connects in a moment.")
}

func (d *demo) removeMachine(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	// A real service checks the machine belongs to the signed-in user.
	if m, err := d.srv.Machine(r.Context(), id); err != nil || m.Owner != owner {
		http.NotFound(w, r)
		return
	}
	if err := d.srv.RemoveMachine(r.Context(), id); err != nil {
		http.Error(w, err.Error(), http.StatusInternalServerError)
		return
	}
	redirect(w, r, "/", "Machine removed.")
}

// Jobs.

// submit turns the form's text into an input file the service serves,
// gives each declared output an upload URL, and submits the job.
func (d *demo) submit(w http.ResponseWriter, r *http.Request) {
	var bundle *moil.Bundle
	for _, b := range d.srv.Bundles() {
		if b.Hash() == r.FormValue("bundle") {
			bundle = b
		}
	}
	if bundle == nil {
		http.Error(w, "unknown bundle", http.StatusBadRequest)
		return
	}
	var params any
	if p := strings.TrimSpace(r.FormValue("params")); p != "" {
		if err := json.Unmarshal([]byte(p), &params); err != nil {
			http.Error(w, "params must be JSON: "+err.Error(), http.StatusBadRequest)
			return
		}
	}

	input := d.addFile(&file{name: "input.txt", data: []byte(r.FormValue("text")), contentType: "text/plain; charset=utf-8"})
	j := &job{Title: r.FormValue("title"), Bundle: bundle, Created: time.Now(), Outputs: map[string]string{}}
	outputs := map[string]moil.Upload{}
	for _, name := range strings.FieldsFunc(r.FormValue("outputs"), func(c rune) bool { return c == ',' || c == ' ' }) {
		token := d.addFile(&file{name: name})
		j.Outputs[name] = token
		outputs[name] = moil.Upload{URL: d.fileURL(token)}
	}

	run, err := d.srv.Submit(r.Context(), moil.Job{
		Bundle:   bundle,
		Title:    j.Title,
		Params:   params,
		Inputs:   map[string]moil.Download{"input.txt": {URL: d.fileURL(input)}},
		Outputs:  outputs,
		Eligible: moil.OwnedBy(owner), // only the demo user's own machines see the text
	})
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	j.Run = run
	d.mu.Lock()
	d.jobs[run.ID()] = j
	d.order = append([]*job{j}, d.order...)
	d.mu.Unlock()
	http.Redirect(w, r, "/jobs/"+run.ID(), http.StatusSeeOther)
}

func (d *demo) jobPage(w http.ResponseWriter, r *http.Request) {
	j := d.job(r.PathValue("id"))
	if j == nil {
		http.NotFound(w, r)
		return
	}
	data := map[string]any{"Job": j, "Running": true}
	select {
	case <-j.Run.Done():
		// Finished: render everything; no need for the live stream.
		res, err := j.Run.Wait(r.Context())
		var events []eventLine
		for ev := range j.Run.Events(r.Context()) {
			events = append(events, describe(ev))
		}
		data["Running"], data["Events"], data["Result"] = false, events, res
		data["Outputs"] = d.outputs(j)
		if err != nil && !errors.Is(err, moil.ErrCancelled) {
			data["Error"] = err.Error()
		}
		if res != nil && len(res.Meta) > 0 {
			data["Meta"] = indentJSON(res.Meta)
		}
	default:
	}
	render(w, "job.html", data)
}

// jobEvents streams a job's events as server-sent events, from the first,
// and ends with an "end" event when the job ends.
func (d *demo) jobEvents(w http.ResponseWriter, r *http.Request) {
	j := d.job(r.PathValue("id"))
	if j == nil {
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-store")
	rc := http.NewResponseController(w)
	for ev := range j.Run.Events(r.Context()) {
		line, _ := json.Marshal(describe(ev))
		fmt.Fprintf(w, "data: %s\n\n", line)
		rc.Flush()
	}
	if r.Context().Err() == nil {
		fmt.Fprint(w, "event: end\ndata: {}\n\n")
		rc.Flush()
	}
}

// eventLine is an event as the job page shows it.
type eventLine struct {
	Time string `json:"time"`
	Kind string `json:"kind"`
	Text string `json:"text"`
}

func describe(ev moil.Event) eventLine {
	var text string
	switch ev.Kind {
	case moil.EventQueued:
		text = "waiting for a machine"
	case moil.EventAssigned:
		text = fmt.Sprintf("attempt %d assigned to %s", ev.Attempt, ev.Machine)
	case moil.EventRetrying:
		text = fmt.Sprintf("attempt %d failed (%s: %s); retrying", ev.Attempt, ev.Err.Code, ev.Err.Message)
	case moil.EventPhase:
		text = ev.Phase
	case moil.EventProgress:
		if ev.Fraction != nil {
			text = fmt.Sprintf("%.0f%% ", *ev.Fraction*100)
		}
		text += ev.Message
	case moil.EventLog:
		text = ev.Level + ": " + ev.Message
	case moil.EventData:
		text = string(ev.Data)
	}
	return eventLine{Time: ev.Time.Format("15:04:05.000"), Kind: string(ev.Kind), Text: text}
}

// Files.

func (d *demo) addFile(f *file) string {
	b := make([]byte, 24)
	rand.Read(b)
	token := base64.RawURLEncoding.EncodeToString(b)
	d.mu.Lock()
	d.files[token] = f
	d.mu.Unlock()
	return token
}

func (d *demo) fileURL(token string) string { return d.public + "/files/" + token }

func (d *demo) getFile(w http.ResponseWriter, r *http.Request) {
	d.mu.Lock()
	f := d.files[r.PathValue("token")]
	d.mu.Unlock()
	if f == nil || (f.data == nil && !f.received) {
		http.NotFound(w, r)
		return
	}
	if f.contentType != "" {
		w.Header().Set("Content-Type", f.contentType)
	}
	w.Header().Set("Content-Disposition", fmt.Sprintf("inline; filename=%q", f.name))
	w.Write(f.data)
}

// putFile receives an output. The runtime sends the bytes as the body.
func (d *demo) putFile(w http.ResponseWriter, r *http.Request) {
	d.mu.Lock()
	f := d.files[r.PathValue("token")]
	d.mu.Unlock()
	if f == nil || f.name == "input.txt" {
		http.NotFound(w, r)
		return
	}
	data, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 64<<20))
	if err != nil {
		http.Error(w, err.Error(), http.StatusRequestEntityTooLarge)
		return
	}
	d.mu.Lock()
	f.data, f.contentType, f.received = data, r.Header.Get("Content-Type"), true
	d.mu.Unlock()
	w.WriteHeader(http.StatusNoContent)
}

type outputLine struct {
	Name, URL string
	Size      int64
	SHA256    string
}

func (d *demo) outputs(j *job) []outputLine {
	res, _ := j.Run.Wait(context.Background())
	if res == nil {
		return nil
	}
	var out []outputLine
	for name, f := range res.Files {
		out = append(out, outputLine{Name: name, URL: d.fileURL(j.Outputs[name]), Size: f.SizeBytes, SHA256: f.SHA256})
	}
	slices.SortFunc(out, func(a, b outputLine) int { return strings.Compare(a.Name, b.Name) })
	return out
}

func (d *demo) job(id string) *job {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.jobs[id]
}

// Templates.

//go:embed templates
var templateFS embed.FS

var pages = map[string]*template.Template{}

func init() {
	funcs := template.FuncMap{
		"short": func(hash string) string { return hash[:12] },
		"ago":   func(t time.Time) string { return time.Since(t).Round(time.Second).String() + " ago" },
	}
	for _, page := range []string{"home.html", "pair.html", "job.html"} {
		pages[page] = template.Must(template.New("layout.html").Funcs(funcs).ParseFS(templateFS, "templates/layout.html", "templates/"+page))
	}
}

func render(w http.ResponseWriter, page string, data any) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	if err := pages[page].Execute(w, data); err != nil {
		slog.Error("rendering "+page, "err", err)
	}
}

func redirect(w http.ResponseWriter, r *http.Request, to, notice string) {
	http.Redirect(w, r, to+"?notice="+template.URLQueryEscaper(notice), http.StatusSeeOther)
}

func indentJSON(raw json.RawMessage) string {
	var v any
	if json.Unmarshal(raw, &v) != nil {
		return string(raw)
	}
	out, _ := json.MarshalIndent(v, "", "  ")
	return string(out)
}
