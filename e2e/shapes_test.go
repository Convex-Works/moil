package e2e

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"reflect"
	"strings"
	"time"
)

// The JSON that moil prints with --json and gives hooks, in the shapes
// spec/machine.md gives it (§2.2 and §3.3). A field tagged omitempty is
// optional: moil leaves it out when absent. Every other field must be
// there, and nothing else may be (see decodeStrict).

// statusJSON is `moil status --json`.
type statusJSON struct {
	Home     string        `json:"home"`
	Agent    *agentJSON    `json:"agent,omitempty"`
	Paused   bool          `json:"paused"`
	Job      *runningJSON  `json:"job,omitempty"`
	LastJob  *endedJSON    `json:"last_job,omitempty"`
	Services []serviceJSON `json:"services"`
	Problems []problemJSON `json:"problems"`
}

type agentJSON struct {
	Host    string `json:"host"`
	Version string `json:"version"`
}

type runningJSON struct {
	Service   string   `json:"service"`
	JobID     string   `json:"job_id"`
	Attempt   int      `json:"attempt"`
	Title     string   `json:"title,omitempty"`
	Bundle    string   `json:"bundle"`
	Phase     string   `json:"phase"`
	Fraction  *float64 `json:"fraction,omitempty"`
	Message   string   `json:"message,omitempty"`
	StartedAt utcTime  `json:"started_at"`
	Stopping  bool     `json:"stopping"`
}

type endedJSON struct {
	Service string `json:"service"`
	JobID   string `json:"job_id"`
	Attempt int    `json:"attempt"`
	// The spec's example of last_job has no title, though job has one;
	// moil gives last_job the job's title too.
	Title   string     `json:"title,omitempty"`
	Bundle  string     `json:"bundle"`
	Outcome string     `json:"outcome"`
	Error   *errorJSON `json:"error,omitempty"`
	EndedAt utcTime    `json:"ended_at"`
}

type errorJSON struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

// serviceJSON is a service, in `moil status --json` and as each element
// of `moil services --json`.
type serviceJSON struct {
	ID         string          `json:"id"`
	Name       string          `json:"name"`
	BaseURL    string          `json:"base_url"`
	Paired     bool            `json:"paired"`
	Connection *connectionJSON `json:"connection,omitempty"`
	Approvals  []approvalJSON  `json:"approvals"`
	Offered    []offeredJSON   `json:"offered,omitempty"`
	Hooks      []hookJSON      `json:"hooks"`
}

type connectionJSON struct {
	State   string   `json:"state"`
	Error   string   `json:"error,omitempty"`
	RetryAt *utcTime `json:"retry_at,omitempty"`
}

type approvalJSON struct {
	Hash       string  `json:"hash"`
	Name       string  `json:"name"`
	Version    string  `json:"version"`
	ApprovedAt utcTime `json:"approved_at"`
}

type offeredJSON struct {
	Hash        string `json:"hash"`
	Name        string `json:"name"`
	Version     string `json:"version"`
	Description string `json:"description,omitempty"`
	Approved    bool   `json:"approved"`
}

// hookJSON is a hook of a service.
type hookJSON struct {
	Bundle   string `json:"bundle"`
	Path     string `json:"path"`
	Log      string `json:"log"`
	Runnable bool   `json:"runnable"`
}

// hookListingJSON is an element of `moil hooks --json`: a hook, with its
// service.
type hookListingJSON struct {
	Service  string `json:"service"`
	Bundle   string `json:"bundle"`
	Path     string `json:"path"`
	Log      string `json:"log"`
	Runnable bool   `json:"runnable"`
}

type problemJSON struct {
	Path    string `json:"path"`
	Message string `json:"message"`
}

// listedJSON is an element of `moil review SERVICE --json`.
type listedJSON struct {
	Hash        string `json:"hash"`
	Name        string `json:"name"`
	Version     string `json:"version"`
	Description string `json:"description,omitempty"`
	Approved    bool   `json:"approved"`
	Offered     bool   `json:"offered"`
}

// reviewJSON is `moil review SERVICE HASH --json`.
type reviewJSON struct {
	Service  serviceRefJSON    `json:"service"`
	Hash     string            `json:"hash"`
	Approved bool              `json:"approved"`
	Files    map[string]string `json:"files"`
	Packages []packageJSON     `json:"packages"`
	Previous *previousJSON     `json:"previous,omitempty"`
}

type serviceRefJSON struct {
	ID      string `json:"id"`
	Name    string `json:"name"`
	BaseURL string `json:"base_url"`
}

type packageJSON struct {
	Name             string `json:"name"`
	Version          string `json:"version,omitempty"`
	Source           string `json:"source"`
	BuildsFromSource bool   `json:"builds_from_source"`
}

type previousJSON struct {
	Hash    string            `json:"hash"`
	Version string            `json:"version"`
	Changes map[string]string `json:"changes"`
}

// The `Label: value` results, as JSON (spec/machine.md §3.1).
type (
	homeJSON struct {
		Home string `json:"home"`
	}
	pausedJSON struct {
		Paused bool `json:"paused"`
	}
	approvedJSON struct {
		Approved string `json:"approved"`
	}
	revokedJSON struct {
		Revoked string `json:"revoked"`
	}
	unpairedJSON struct {
		Unpaired string `json:"unpaired"`
	}
	pairedJSON struct {
		Paired string `json:"paired"`
	}
	pairingCodeJSON struct {
		Code string `json:"code"`
		URL  string `json:"url"`
	}
	// stoppingJSON is `moil stop --json`: `{}` when no job runs.
	stoppingJSON struct {
		Stopping string `json:"stopping,omitempty"`
	}
	// nothingJSON is what a command prints that has no result to print,
	// such as `moil quit --json`.
	nothingJSON struct{}
)

// hookContextJSON is what a hook reads on stdin (spec/machine.md §2.2).
type hookContextJSON struct {
	V         int               `json:"v"`
	Service   serviceRefJSON    `json:"service"`
	Bundle    bundleRefJSON     `json:"bundle"`
	JobID     string            `json:"job_id"`
	Attempt   int               `json:"attempt"`
	Title     string            `json:"title,omitempty"`
	Params    json.RawMessage   `json:"params"`
	Meta      json.RawMessage   `json:"meta,omitempty"`
	EndedAt   utcTime           `json:"ended_at"`
	OutputDir string            `json:"output_dir"`
	Outputs   map[string]string `json:"outputs"`
}

type bundleRefJSON struct {
	Name    string `json:"name"`
	Version string `json:"version"`
	Hash    string `json:"hash"`
}

// A utcTime is a time as moil writes them: an RFC 3339 string in UTC, such
// as 2026-09-27T22:41:07Z (spec/machine.md §1).
type utcTime struct{ time.Time }

func (u *utcTime) UnmarshalJSON(data []byte) error {
	var s string
	if err := json.Unmarshal(data, &s); err != nil {
		return err
	}
	t, err := time.Parse(time.RFC3339, s)
	if err != nil || !strings.HasSuffix(s, "Z") {
		return fmt.Errorf("%q isn't an RFC 3339 time in UTC", s)
	}
	u.Time = t
	return nil
}

// decodeStrict decodes the one JSON value in data into v, afresh. It fails if the
// value has a field v's type doesn't, lacks one that isn't optional, or
// has a null anywhere but in what a service or script controls, such as
// params: optional fields are left out when absent (spec/machine.md
// §3.1).
func decodeStrict(data []byte, v any) error {
	// What v held before mustn't show through where data has nothing.
	reflect.ValueOf(v).Elem().SetZero()
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return err
	}
	if _, err := dec.Token(); !errors.Is(err, io.EOF) {
		return errors.New("more follows the JSON value")
	}
	var value any
	dec = json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber()
	if err := dec.Decode(&value); err != nil {
		return err
	}
	return checkShape(value, reflect.TypeOf(v).Elem(), "$")
}

var (
	rawMessageType  = reflect.TypeFor[json.RawMessage]()
	unmarshalerType = reflect.TypeFor[json.Unmarshaler]()
)

// checkShape reports a null in value, or a field of t missing from it
// that isn't optional.
func checkShape(value any, t reflect.Type, path string) error {
	if t == rawMessageType {
		return nil
	}
	if value == nil {
		return fmt.Errorf("%s is null; an absent field is left out instead", path)
	}
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	if reflect.PointerTo(t).Implements(unmarshalerType) {
		return nil
	}
	switch t.Kind() {
	case reflect.Struct:
		object, ok := value.(map[string]any)
		if !ok {
			return fmt.Errorf("%s is %v, not an object", path, value)
		}
		for i := range t.NumField() {
			field := t.Field(i)
			name, options, _ := strings.Cut(field.Tag.Get("json"), ",")
			member, ok := object[name]
			if !ok {
				if options != "omitempty" {
					return fmt.Errorf("%s has no %s", path, name)
				}
				continue
			}
			if err := checkShape(member, field.Type, path+"."+name); err != nil {
				return err
			}
		}
	case reflect.Slice:
		elements, _ := value.([]any)
		for i, element := range elements {
			if err := checkShape(element, t.Elem(), fmt.Sprintf("%s[%d]", path, i)); err != nil {
				return err
			}
		}
	case reflect.Map:
		members, _ := value.(map[string]any)
		for name, member := range members {
			if err := checkShape(member, t.Elem(), path+"."+name); err != nil {
				return err
			}
		}
	}
	return nil
}
