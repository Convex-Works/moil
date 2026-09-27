package wire

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

const vectorsDir = "../../../../spec/vectors"

type messageVectors struct {
	MachineToService []namedMessage             `json:"machine_to_service"`
	ServiceToMachine []namedMessage             `json:"service_to_machine"`
	HTTP             map[string]json.RawMessage `json:"http"`
}

type namedMessage struct {
	Name    string          `json:"name"`
	Message json.RawMessage `json:"message"`
}

func loadMessageVectors(t *testing.T) messageVectors {
	t.Helper()
	if _, err := os.Stat(vectorsDir); err != nil {
		t.Skipf("the shared test vectors aren't at %s (%v); run the SDK's tests from a checkout of the moil repository", vectorsDir, err)
	}
	data, err := os.ReadFile(filepath.Join(vectorsDir, "messages.json"))
	if err != nil {
		t.Fatal(err)
	}
	var v messageVectors
	if err := json.Unmarshal(data, &v); err != nil {
		t.Fatal(err)
	}
	return v
}

// Every control message in the vectors decodes to its typed form and
// encodes back to an equal JSON value, so the Go SDK and the Rust machine
// read and write the same bytes.
func TestControlMessageVectorsRoundTrip(t *testing.T) {
	v := loadMessageVectors(t)
	cases := append(v.MachineToService, v.ServiceToMachine...)
	if len(cases) < 20 {
		t.Fatalf("only %d message vectors; the file looks truncated", len(cases))
	}
	for _, c := range cases {
		t.Run(c.Name, func(t *testing.T) {
			m, err := Decode(c.Message)
			if err != nil {
				t.Fatalf("decode: %v", err)
			}
			if u, ok := m.(Unknown); ok {
				t.Fatalf("decoded as unknown type %q", u.Name)
			}
			out, err := Encode(m)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			assertSameJSON(t, c.Message, out)
			if !bytes.HasPrefix(out, []byte(`{"type":`)) {
				t.Errorf("encoded message doesn't lead with its type: %s", out)
			}
		})
	}
}

// The HTTP bodies decode strictly (every field in the vectors is modelled)
// and encode back to equal JSON values.
func TestHTTPBodyVectorsRoundTrip(t *testing.T) {
	v := loadMessageVectors(t)
	bodies := map[string]func() any{
		"info_response":    func() any { return new(InfoResponse) },
		"pair_request":     func() any { return new(PairRequest) },
		"pair_response":    func() any { return new(PairResponse) },
		"token_request":    func() any { return new(TokenRequest) },
		"token_response":   func() any { return new(TokenResponse) },
		"bundles_response": func() any { return new(BundlesResponse) },
	}
	var errorBodies []json.RawMessage
	if err := json.Unmarshal(v.HTTP["error_responses"], &errorBodies); err != nil {
		t.Fatal(err)
	}
	type body struct {
		name string
		raw  json.RawMessage
		new  func() any
	}
	var all []body
	for name, raw := range v.HTTP {
		if name == "error_responses" {
			continue
		}
		newBody, ok := bodies[name]
		if !ok {
			t.Errorf("vector %q has no Go type", name)
			continue
		}
		all = append(all, body{name, raw, newBody})
	}
	for i, raw := range errorBodies {
		all = append(all, body{fmt.Sprintf("error_responses[%d]", i), raw, func() any { return new(ErrorBody) }})
	}
	for _, b := range all {
		t.Run(b.name, func(t *testing.T) {
			dec := json.NewDecoder(bytes.NewReader(b.raw))
			dec.DisallowUnknownFields()
			value := b.new()
			if err := dec.Decode(value); err != nil {
				t.Fatalf("decode: %v", err)
			}
			out, err := json.Marshal(value)
			if err != nil {
				t.Fatal(err)
			}
			assertSameJSON(t, b.raw, out)
		})
	}
}

func TestDecodeRejectsWhatThePeerMustNotSend(t *testing.T) {
	hash := strings.Repeat("a", 64)
	for _, c := range []struct{ name, msg string }{
		{"not JSON", `hello`},
		{"not an object", `["hello"]`},
		{"null", `null`},
		{"no type", `{"job_id": "j"}`},
		{"type not a string", `{"type": 1}`},
		{"missing required field", `{"type": "bid"}`},
		{"required field null", `{"type": "state", "state": null}`},
		{"hello from another protocol version", `{"type": "hello", "protocol": 2, "app_version": "9", "machine": {"name": "m", "os": "linux", "arch": "x86_64", "cpus": 1, "memory_bytes": 1, "gpus": []}, "state": "idle", "approved": [], "attempts": []}`},
		{"unknown state", `{"type": "state", "state": "sleeping"}`},
		{"invalid hash", `{"type": "approved", "hashes": ["ABC"]}`},
		{"job ID that escapes", `{"type": "bid", "job_id": "../x"}`},
		{"attempt zero", `{"type": "renew", "job_id": "j", "attempt": 0}`},
		{"seq zero", `{"type": "event", "job_id": "j", "attempt": 1, "seq": 0, "event": {"v": 1, "type": "log", "message": "x"}}`},
		{"fraction above one", `{"type": "event", "job_id": "j", "attempt": 1, "seq": 1, "event": {"type": "progress", "fraction": 1.5}}`},
		{"empty progress", `{"type": "event", "job_id": "j", "attempt": 1, "seq": 1, "event": {"type": "progress"}}`},
		{"data without payload", `{"type": "event", "job_id": "j", "attempt": 1, "seq": 1, "event": {"type": "data"}}`},
		{"succeeded without result", `{"type": "done", "job_id": "j", "attempt": 1, "seq": 1, "outcome": "succeeded"}`},
		{"failed without error", `{"type": "done", "job_id": "j", "attempt": 1, "seq": 1, "outcome": "failed"}`},
		{"unknown outcome", `{"type": "done", "job_id": "j", "attempt": 1, "seq": 1, "outcome": "exploded"}`},
		{"assign without params", `{"type": "assign", "job_id": "j", "attempt": 1, "bundle_hash": "` + hash + `", "inputs": {}, "outputs": {}, "lease_ms": 1}`},
		{"assign with an unsafe output name", `{"type": "assign", "job_id": "j", "attempt": 1, "bundle_hash": "` + hash + `", "params": null, "inputs": {}, "outputs": {"../x": {"url": "https://e.com", "method": "PUT"}}, "lease_ms": 1}`},
		{"attempt as a string", `{"type": "ack", "job_id": "j", "attempt": "1"}`},
	} {
		t.Run(c.name, func(t *testing.T) {
			if m, err := Decode([]byte(c.msg)); err == nil {
				t.Fatalf("accepted %s as %#v", c.msg, m)
			}
		})
	}
}

// Receivers ignore unknown fields and unknown message types, so that newer
// peers can add them (spec §2, §11).
func TestDecodeToleratesAdditions(t *testing.T) {
	m, err := Decode([]byte(`{"type": "bid", "job_id": "j_1", "fitness": 0.9}`))
	if err != nil {
		t.Fatal(err)
	}
	if m != (Bid{JobID: "j_1"}) {
		t.Fatalf("got %#v", m)
	}
	m, err = Decode([]byte(`{"type": "telemetry", "cpu": 0.5}`))
	if err != nil {
		t.Fatal(err)
	}
	if m != (Unknown{Name: "telemetry"}) {
		t.Fatalf("got %#v", m)
	}
	m, err = Decode([]byte(`{"type": "event", "job_id": "j", "attempt": 1, "seq": 1, "event": {"v": 1, "type": "metric", "gpu": 0.5}}`))
	if err != nil {
		t.Fatalf("an event of a newer type must decode: %v", err)
	}
	if ev := m.(Event); ev.Event.Type != "metric" {
		t.Fatalf("got %#v", ev)
	}
}

// Collections the Rust side requires are sent as [] or {}, never null,
// even when the Go value was left nil.
func TestEncodeNeverSendsNullCollections(t *testing.T) {
	for _, m := range []Message{
		Welcome{Protocol: 1, MachineID: "m_1", Service: ServiceInfo{Name: "s"}},
		Bundles{},
		Assign{JobID: "j", Attempt: 1, BundleHash: strings.Repeat("a", 64), LeaseMS: 1},
		Hello{Protocol: 1, State: StateIdle},
		Done{JobID: "j", Attempt: 1, Seq: 1, Outcome: OutcomeSucceeded, Result: &Result{}},
	} {
		out, err := Encode(m)
		if err != nil {
			t.Fatal(err)
		}
		if bytes.Contains(out, []byte("null")) && m.Type() != "assign" {
			t.Errorf("%s: %s", m.Type(), out)
		}
		if _, err := Decode(out); err != nil {
			t.Errorf("%s doesn't decode after encoding: %v\n%s", m.Type(), err, out)
		}
	}
}

// assertSameJSON compares two JSON documents as values: object key order
// doesn't matter and numbers compare by value, so 1 equals 1.0.
func assertSameJSON(t *testing.T, want, got []byte) {
	t.Helper()
	w, err := decodeNumbers(want)
	if err != nil {
		t.Fatalf("want: %v", err)
	}
	g, err := decodeNumbers(got)
	if err != nil {
		t.Fatalf("got: %v", err)
	}
	if !sameJSON(w, g) {
		t.Errorf("JSON differs\nwant: %s\n got: %s", compact(want), got)
	}
}

func decodeNumbers(data []byte) (any, error) {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	var v any
	err := dec.Decode(&v)
	return v, err
}

func sameJSON(a, b any) bool {
	switch a := a.(type) {
	case json.Number:
		b, ok := b.(json.Number)
		if !ok {
			return false
		}
		x, okx := new(big.Rat).SetString(a.String())
		y, oky := new(big.Rat).SetString(b.String())
		return okx && oky && x.Cmp(y) == 0
	case map[string]any:
		b, ok := b.(map[string]any)
		if !ok || len(a) != len(b) {
			return false
		}
		for k, av := range a {
			bv, ok := b[k]
			if !ok || !sameJSON(av, bv) {
				return false
			}
		}
		return true
	case []any:
		b, ok := b.([]any)
		if !ok || len(a) != len(b) {
			return false
		}
		for i := range a {
			if !sameJSON(a[i], b[i]) {
				return false
			}
		}
		return true
	default:
		return reflect.DeepEqual(a, b)
	}
}

func compact(data []byte) string {
	var buf bytes.Buffer
	if err := json.Compact(&buf, data); err != nil {
		return string(data)
	}
	return buf.String()
}

func TestSameJSONComparesNumbersByValue(t *testing.T) {
	for _, c := range []struct {
		a, b string
		same bool
	}{
		{`{"a": 1, "b": [1.0, 2]}`, `{"b": [1, 2.0], "a": 1.00}`, true},
		{`{"a": 1}`, `{"a": 1.5}`, false},
		{`{"a": null}`, `{}`, false},
		{`[1, 2]`, `[2, 1]`, false},
		{`{"a": "1"}`, `{"a": 1}`, false},
	} {
		a, _ := decodeNumbers([]byte(c.a))
		b, _ := decodeNumbers([]byte(c.b))
		if sameJSON(a, b) != c.same {
			t.Errorf("sameJSON(%s, %s) = %v", c.a, c.b, !c.same)
		}
	}
}
