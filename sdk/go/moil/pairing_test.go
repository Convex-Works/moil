package moil_test

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"

	"git.convex.works/ConvexWorks/moil/sdk/go/moil"
	"git.convex.works/ConvexWorks/moil/sdk/go/moiltest"
)

// post sends a JSON body and decodes the JSON answer.
func post(t *testing.T, url string, body any) (int, map[string]any) {
	t.Helper()
	return request(t, http.MethodPost, url, "", body)
}

func request(t *testing.T, method, url, token string, body any) (int, map[string]any) {
	t.Helper()
	var r io.Reader
	if body != nil {
		r = bytes.NewReader(mustJSON(t, body))
	}
	req, err := http.NewRequest(method, url, r)
	if err != nil {
		t.Fatal(err)
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var out map[string]any
	if resp.StatusCode != http.StatusNoContent {
		if err := json.NewDecoder(resp.Body).Decode(&out); err != nil {
			t.Fatalf("%s %s answered %d without JSON: %v", method, url, resp.StatusCode, err)
		}
	}
	return resp.StatusCode, out
}

func startPairing(t *testing.T, base string) (deviceCode, userCode string) {
	t.Helper()
	status, resp := post(t, base+"/v1/pair", map[string]string{
		"name": "Ada's MacBook Pro", "os": "macos", "arch": "aarch64", "app_version": "0.1.0",
	})
	if status != http.StatusOK {
		t.Fatalf("POST /v1/pair: %d %v", status, resp)
	}
	return resp["device_code"].(string), resp["user_code"].(string)
}

func pollToken(t *testing.T, base, deviceCode string) (int, map[string]any) {
	t.Helper()
	return post(t, base+"/v1/pair/token", map[string]string{"device_code": deviceCode})
}

func pollError(t *testing.T, base, deviceCode string) string {
	t.Helper()
	status, resp := pollToken(t, base, deviceCode)
	if status != http.StatusBadRequest {
		t.Fatalf("poll answered %d %v, want an RFC 8628 error", status, resp)
	}
	return resp["error"].(string)
}

// The device flow of spec §4, as the app runs it and as a service's
// confirmation page drives it.
func TestPairingConfirmedEndToEnd(t *testing.T) {
	storePath := filepath.Join(t.TempDir(), "machines.json")
	store, err := moil.NewFileStore(storePath)
	if err != nil {
		t.Fatal(err)
	}
	srv := newServer(t, func(c *moil.Config) { c.Store = store })
	base := moiltest.Serve(t, srv)

	status, info := request(t, http.MethodGet, base+"/v1/info", "", nil)
	if status != http.StatusOK || info["protocol"] != 1.0 || info["name"] != "klisi" {
		t.Fatalf("GET /v1/info: %d %v", status, info)
	}

	status, resp := post(t, base+"/v1/pair", map[string]string{
		"name": "Ada's MacBook Pro", "os": "macos", "arch": "aarch64", "app_version": "0.1.0",
	})
	if status != http.StatusOK {
		t.Fatalf("POST /v1/pair: %d %v", status, resp)
	}
	userCode, deviceCode := resp["user_code"].(string), resp["device_code"].(string)
	if !regexp.MustCompile(`^[BCDFGHJKLMNPQRSTVWXZ]{4}-[BCDFGHJKLMNPQRSTVWXZ]{4}$`).MatchString(userCode) {
		t.Errorf("user code %q isn't 8 letters of the spec's alphabet around a hyphen", userCode)
	}
	if len(deviceCode) < 40 {
		t.Errorf("device code %q is too short to resist guessing", deviceCode)
	}
	if resp["verification_uri"] != "https://klisi.example.com/machines/pair" ||
		resp["verification_uri_complete"] != "https://klisi.example.com/machines/pair?code="+userCode ||
		resp["expires_in"] != 600.0 || resp["interval"] != 1.0 || resp["service"].(map[string]any)["name"] != "klisi" {
		t.Errorf("pair response %v", resp)
	}

	if got := pollError(t, base, deviceCode); got != "authorization_pending" {
		t.Fatalf("before confirmation: %q", got)
	}

	// The confirmation page: users type codes loosely.
	loose := strings.ToLower(strings.ReplaceAll(userCode, "-", " "))
	p, ok := srv.PendingPairing(loose)
	if !ok || p.Code != userCode || p.Name != "Ada's MacBook Pro" || p.OS != "macos" || p.Arch != "aarch64" {
		t.Fatalf("PendingPairing(%q) = %+v, %v", loose, p, ok)
	}
	m, err := srv.ConfirmPairing(context.Background(), loose, "alice")
	if err != nil {
		t.Fatal(err)
	}
	if m.Owner != "alice" || m.Name != "Ada's MacBook Pro" || m.Online() {
		t.Errorf("confirmed machine %+v", m)
	}
	if _, ok := srv.PendingPairing(userCode); ok {
		t.Error("a confirmed pairing is still pending")
	}
	if _, err := srv.ConfirmPairing(context.Background(), userCode, "mallory"); !errors.Is(err, moil.ErrUnknownCode) {
		t.Errorf("confirming twice: %v", err)
	}
	listed, err := srv.Machines(context.Background(), "alice")
	if err != nil || len(listed) != 1 || listed[0].ID != m.ID {
		t.Fatalf("Machines(alice) = %+v, %v", listed, err)
	}

	status, resp = pollToken(t, base, deviceCode)
	if status != http.StatusOK || resp["machine_id"] != m.ID {
		t.Fatalf("after confirmation: %d %v", status, resp)
	}
	token := resp["token"].(string)
	if len(token) < 40 {
		t.Errorf("token %q is too short", token)
	}
	if status, _ := request(t, http.MethodGet, base+"/v1/bundles", token, nil); status != http.StatusOK {
		t.Errorf("the new token doesn't authenticate: %d", status)
	}
	if got := pollError(t, base, deviceCode); got != "expired_token" {
		t.Errorf("a device code must yield its token once; second poll: %q", got)
	}

	// Only the token's hash is stored.
	saved, err := os.ReadFile(storePath)
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256([]byte(token))
	if bytes.Contains(saved, []byte(token)) || !bytes.Contains(saved, []byte(hex.EncodeToString(sum[:]))) {
		t.Errorf("the store must hold the token's SHA-256 and never the token:\n%s", saved)
	}
}

func TestPairingDenied(t *testing.T) {
	srv := newServer(t)
	base := moiltest.Serve(t, srv)
	deviceCode, userCode := startPairing(t, base)
	if err := srv.DenyPairing(userCode); err != nil {
		t.Fatal(err)
	}
	if got := pollError(t, base, deviceCode); got != "access_denied" {
		t.Fatalf("after denial: %q", got)
	}
	if _, err := srv.ConfirmPairing(context.Background(), userCode, "alice"); !errors.Is(err, moil.ErrUnknownCode) {
		t.Fatalf("confirming a denied pairing: %v", err)
	}
	if err := srv.DenyPairing("BCDF-GHJK"); !errors.Is(err, moil.ErrUnknownCode) {
		t.Fatalf("denying an unknown code: %v", err)
	}
	if ms, _ := srv.Machines(context.Background(), "alice"); len(ms) != 0 {
		t.Fatalf("a denied machine was stored: %+v", ms)
	}
}

func TestPairingExpires(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.PairingTTL = 50 * time.Millisecond })
	base := moiltest.Serve(t, srv)
	deviceCode, userCode := startPairing(t, base)
	if _, ok := srv.PendingPairing(userCode); !ok {
		t.Fatal("a fresh pairing isn't pending")
	}
	eventually(t, "the pairing to expire", func() bool { _, ok := srv.PendingPairing(userCode); return !ok })
	if _, err := srv.ConfirmPairing(context.Background(), userCode, "alice"); !errors.Is(err, moil.ErrUnknownCode) {
		t.Fatalf("confirming an expired code: %v", err)
	}
	if got := pollError(t, base, deviceCode); got != "expired_token" {
		t.Fatalf("polling an expired code: %q", got)
	}
}

// A machine polling faster than the interval is told to slow down; one
// polling at the interval isn't.
func TestPairingSlowDown(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.PairingInterval = 100 * time.Millisecond })
	base := moiltest.Serve(t, srv)
	deviceCode, _ := startPairing(t, base)
	if got := pollError(t, base, deviceCode); got != "authorization_pending" {
		t.Fatalf("first poll: %q", got)
	}
	if got := pollError(t, base, deviceCode); got != "slow_down" {
		t.Fatalf("immediate second poll: %q", got)
	}
	time.Sleep(100 * time.Millisecond) // the behavior under test is the interval passing
	if got := pollError(t, base, deviceCode); got != "authorization_pending" {
		t.Fatalf("poll after the interval: %q", got)
	}
	if got := pollError(t, base, "not-a-device-code"); got != "expired_token" {
		t.Fatalf("unknown device code: %q", got)
	}
}

// Anonymous pairing requests can't pile up without bound, nor lock
// others out of pairing: a new pairing replaces the oldest one nobody
// confirmed. A confirmed one waits for its machine.
func TestPendingPairingsAreBounded(t *testing.T) {
	srv := newServer(t, func(c *moil.Config) { c.MaxPendingPairings = 2 })
	base := moiltest.Serve(t, srv)
	firstDevice, first := startPairing(t, base)
	secondDevice, second := startPairing(t, base)
	_, third := startPairing(t, base)
	if _, ok := srv.PendingPairing(first); ok {
		t.Error("the oldest pairing survived a third on a limit of two")
	}
	if got := pollError(t, base, firstDevice); got != "expired_token" {
		t.Errorf("the replaced pairing's machine was told %q", got)
	}
	if _, err := srv.ConfirmPairing(context.Background(), second, "alice"); err != nil {
		t.Fatal(err)
	}
	startPairing(t, base) // replaces the third, not the confirmed second
	if _, ok := srv.PendingPairing(third); ok {
		t.Error("an unconfirmed pairing survived while a confirmed one was replaced")
	}
	if status, resp := pollToken(t, base, secondDevice); status != http.StatusOK || resp["token"] == nil {
		t.Errorf("the confirmed machine couldn't collect its token: %d %v", status, resp)
	}

	// With every slot confirmed, there's nothing to replace.
	full := newServer(t, func(c *moil.Config) { c.MaxPendingPairings = 1 })
	fullBase := moiltest.Serve(t, full)
	_, code := startPairing(t, fullBase)
	if _, err := full.ConfirmPairing(context.Background(), code, "alice"); err != nil {
		t.Fatal(err)
	}
	status, resp := post(t, fullBase+"/v1/pair", map[string]string{"name": "another", "os": "linux", "arch": "x86_64", "app_version": "0.1.0"})
	if status != http.StatusServiceUnavailable || resp["error"] != "temporarily_unavailable" {
		t.Fatalf("pairing with every slot confirmed: %d %v", status, resp)
	}
}

// gatedStore holds AddMachine until released.
type gatedStore struct {
	moil.Store
	release chan struct{}
}

func (s gatedStore) AddMachine(ctx context.Context, m moil.MachineRecord) error {
	<-s.release
	return s.Store.AddMachine(ctx, m)
}

// A pairing confirmed as its code expires still reaches its machine, so
// the Store doesn't keep a machine nobody can connect as.
func TestPairingConfirmedAsItExpires(t *testing.T) {
	store := gatedStore{Store: moil.NewMemoryStore(), release: make(chan struct{})}
	srv := newServer(t, func(c *moil.Config) { c.Store = store; c.PairingTTL = 100 * time.Millisecond })
	base := moiltest.Serve(t, srv)
	deviceCode, userCode := startPairing(t, base)
	p, _ := srv.PendingPairing(userCode)
	confirmed := make(chan error, 1)
	go func() {
		_, err := srv.ConfirmPairing(context.Background(), userCode, "alice")
		confirmed <- err
	}()
	eventually(t, "the code to expire", func() bool { return time.Now().After(p.Expires) })
	if got := pollError(t, base, deviceCode); got != "authorization_pending" {
		t.Fatalf("while the Store saves the machine, its code is %q", got)
	}
	close(store.release)
	if err := <-confirmed; err != nil {
		t.Fatal(err)
	}
	status, resp := pollToken(t, base, deviceCode)
	if status != http.StatusOK {
		t.Fatalf("the confirmed machine polled after its code's expiry: %d %v", status, resp)
	}
	if _, err := srv.Machine(context.Background(), resp["machine_id"].(string)); err != nil {
		t.Fatal(err)
	}
}

func TestPairRequestsAreChecked(t *testing.T) {
	srv := newServer(t)
	base := moiltest.Serve(t, srv)
	for _, body := range []any{
		map[string]string{"name": ""},
		map[string]string{"name": strings.Repeat("é", 65)},
		map[string]string{"name": "ok", "os": strings.Repeat("x", 65)},
		"not an object",
	} {
		if status, resp := post(t, base+"/v1/pair", body); status != http.StatusBadRequest || resp["error"] != "invalid_request" {
			t.Errorf("%v: %d %v", body, status, resp)
		}
	}
	if status, _ := post(t, base+"/v1/pair", map[string]string{"name": strings.Repeat("é", 64)}); status != http.StatusOK {
		t.Errorf("a 64-character name was refused: %d", status)
	}
}

// The service's own confirmation page drives pairing; moiltest can play
// the machine against it.
func TestMoiltestPairsThroughTheServicePage(t *testing.T) {
	mux := http.NewServeMux()
	site := httptestServer(t, mux)
	srv := newServer(t, func(c *moil.Config) { c.VerificationURL = site + "/machines/pair" })
	mux.Handle("/moil/", http.StripPrefix("/moil", srv.Handler()))
	mux.HandleFunc("POST /machines/pair", func(w http.ResponseWriter, r *http.Request) {
		// A real page would take the owner from the session.
		if _, err := srv.ConfirmPairing(r.Context(), r.FormValue("code"), "alice"); err != nil {
			http.Error(w, err.Error(), http.StatusNotFound)
		}
	})

	m := moiltest.New(t, site+"/moil", moiltest.WithName("Ada's desktop"))
	code := m.StartPairing()
	var perr *moiltest.PairingError
	if err := m.TryFinishPairing(); !errors.As(err, &perr) || perr.Code != "authorization_pending" {
		t.Fatalf("before confirming: %v", err)
	}
	resp, err := http.PostForm(site+"/machines/pair", url.Values{"code": {code}})
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("confirming on the page: %v %v", resp, err)
	}
	resp.Body.Close()
	m.FinishPairing()
	m.Connect()
	if got := machine(t, srv, m.ID()); got.Owner != "alice" || got.Name != "Ada's desktop" || !got.Online() {
		t.Fatalf("paired machine %+v", got)
	}
}

// Like a machine, moiltest refuses to pair when the service's
// confirmation page isn't on the origin it reaches the service at, so
// that the service's own tests catch the mistake (spec §4).
func TestMoiltestRefusesAConfirmationPageElsewhere(t *testing.T) {
	mux := http.NewServeMux()
	site := httptestServer(t, mux)
	srv := newServer(t) // its page is on https://klisi.example.com
	mux.Handle("/moil/", http.StripPrefix("/moil", srv.Handler()))

	want := `moiltest: machines refuse to pair: the confirmation page "https://klisi.example.com/machines/pair" is on https://klisi.example.com:443, not on ` +
		site + `, the origin of the base URL (spec §4)`
	tb := &failureRecorder{TB: t}
	for name, pair := range map[string]func(){
		"New":     func() { moiltest.New(tb, site+"/moil").StartPairing() },
		"Pair At": func() { moiltest.Pair(tb, srv, "alice", moiltest.At(site+"/moil")) },
	} {
		if got := tb.failure(pair); got != want {
			t.Errorf("%s failed the test with %q, want %q", name, got, want)
		}
	}
	if ms, _ := srv.Machines(context.Background(), "alice"); len(ms) != 0 {
		t.Errorf("paired %+v", ms)
	}
}
