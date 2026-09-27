package moiltest

import (
	"math"
	"testing"
)

// Numbers come out as serde_json, and so the app, writes them.
func TestFormatDouble(t *testing.T) {
	for f, want := range map[float64]string{
		1:                      "1.0",
		100:                    "100.0",
		0.93:                   "0.93",
		1.25:                   "1.25",
		-2.5:                   "-2.5",
		0.001234:               "0.001234",
		1e-5:                   "0.00001",
		1e-6:                   "1e-6",
		3.5e-7:                 "3.5e-7",
		1e15:                   "1000000000000000.0",
		1e16:                   "1e+16",
		1.2345678901234568e17:  "1.2345678901234568e+17",
		1e300:                  "1e+300",
		5e-324:                 "5e-324",
		math.MaxFloat64:        "1.7976931348623157e+308",
		0:                      "0.0",
		math.Copysign(0, -1):   "-0.0",
		18446744073709551616.0: "1.8446744073709552e+19",
		-0.0000123:             "-0.0000123",
	} {
		if got := formatDouble(f); got != want {
			t.Errorf("formatDouble(%v) = %s, want %s", f, got, want)
		}
	}
}

// Values come out as serde_json writes them: keys in order, no
// whitespace, and only quotes, backslashes and control characters
// escaped.
func TestReencode(t *testing.T) {
	escape := func(hex string) string { return `\` + "u" + hex }
	in := `{ "b": [1, 1.0, 1e2, -0, 18446744073709551615, -9223372036854775809, 1.5E+3],
	         "a": "<é>` + escape("2028") + `\"\\\n` + escape("0001") + "\x7f" + `", "c": {"z": null, "y": true} }`
	want := `{"a":"<é>` + "\xe2\x80\xa8" + `\"\\\n` + escape("0001") + "\x7f" + `","b":[1,1.0,100.0,-0.0,18446744073709551615,-9.223372036854776e+18,1500.0],"c":{"y":true,"z":null}}`
	got, err := reencode([]byte(in))
	if err != nil || string(got) != want {
		t.Fatalf("reencode = %s, %v\nwant %s", got, err, want)
	}
	if _, err := reencode([]byte(`[1e400]`)); err == nil {
		t.Fatal("a number beyond a double was re-encoded")
	}
}
