package e2e

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// What every test shares, set up once by TestMain.
var (
	// repoRoot is the repository's root directory.
	repoRoot string
	// moilBin is the moil binary built from this repository.
	moilBin string
	// uvBin is what machines run as uv: the real uv, with a uv cache of
	// the suite's own (see useUV).
	uvBin string
)

func TestMain(m *testing.M) {
	cleanup, err := setUp()
	if err != nil {
		fmt.Fprintln(os.Stderr, "e2e:", err)
		os.Exit(1)
	}
	code := m.Run()
	cleanup()
	os.Exit(code)
}

func setUp() (cleanup func(), err error) {
	if repoRoot, err = filepath.Abs(".."); err != nil {
		return nil, err
	}
	uv, err := findUV()
	if err != nil {
		return nil, err
	}
	if moilBin, err = buildMoil(); err != nil {
		return nil, err
	}
	scratch, err := os.MkdirTemp("", "moil-e2e-")
	if err != nil {
		return nil, err
	}
	if uvBin, err = useUV(uv, scratch); err != nil {
		os.RemoveAll(scratch)
		return nil, err
	}
	return func() { os.RemoveAll(scratch) }, nil
}

// findUV finds the real uv: on PATH, or where its installer puts it.
func findUV() (string, error) {
	if path, err := exec.LookPath("uv"); err == nil {
		return filepath.Abs(path)
	}
	home, _ := os.UserHomeDir()
	installed := filepath.Join(home, ".local", "bin", "uv")
	if _, err := os.Stat(installed); err == nil {
		return installed, nil
	}
	return "", fmt.Errorf("these tests run bundles with real uv, which isn't on PATH or in ~/.local/bin; install it: https://docs.astral.sh/uv/")
}

// buildMoil builds the moil binary from this repository and returns its
// path.
func buildMoil() (string, error) {
	build := exec.Command("cargo", "build", "-p", "moil-cli")
	build.Dir = repoRoot
	build.Stdout, build.Stderr = os.Stderr, os.Stderr
	if err := build.Run(); err != nil {
		return "", fmt.Errorf("building moil: %w", err)
	}
	out, err := exec.Command("cargo", "metadata", "--no-deps", "--format-version", "1", "--manifest-path", filepath.Join(repoRoot, "Cargo.toml")).Output()
	if err != nil {
		return "", fmt.Errorf("finding cargo's target directory: %w", err)
	}
	var metadata struct {
		TargetDirectory string `json:"target_directory"`
	}
	if err := json.Unmarshal(out, &metadata); err != nil {
		return "", fmt.Errorf("reading cargo metadata: %w", err)
	}
	return filepath.Join(metadata.TargetDirectory, "debug", "moil"), nil
}

// useUV returns a uv for machines to run ($MOIL_UV): the real one, with a
// uv cache in scratch. The runtime passes uv only a minimal environment,
// so without this every test home's environments would pile up in the
// developer's own uv cache. Installed Pythons are shared regardless.
func useUV(uv, scratch string) (string, error) {
	wrapper := filepath.Join(scratch, "uv")
	script := fmt.Sprintf("#!/bin/sh\nexport UV_CACHE_DIR=%s\nexec %s \"$@\"\n", shellQuote(filepath.Join(scratch, "uv-cache")), shellQuote(uv))
	if err := os.WriteFile(wrapper, []byte(script), 0o755); err != nil {
		return "", err
	}
	return wrapper, nil
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}
