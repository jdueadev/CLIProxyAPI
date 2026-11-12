package configaccess

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"testing"

	sdkaccess "github.com/router-for-me/CLIProxyAPI/v8/sdk/access"
	sdkconfig "github.com/router-for-me/CLIProxyAPI/v8/sdk/config"
)

func authenticate(t *testing.T, key string) *sdkaccess.AuthError {
	t.Helper()
	var found sdkaccess.Provider
	for _, p := range sdkaccess.RegisteredProviders() {
		if p.Identifier() == sdkaccess.DefaultAccessProviderName {
			found = p
		}
	}
	if found == nil {
		t.Fatalf("config access provider is not registered")
	}
	r := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	r.Header.Set("Authorization", "Bearer "+key)
	_, err := found.Authenticate(context.Background(), r)
	return err
}

func TestFileKeysWorkWhenTheVariableIsUnset(t *testing.T) {
	os.Unsetenv(EnvAPIKeys)
	Register(&sdkconfig.SDKConfig{APIKeys: []string{"file-key"}})
	t.Cleanup(func() { Register(nil) })
	if err := authenticate(t, "file-key"); err != nil {
		t.Fatalf("file key refused: %v", err)
	}
	if err := authenticate(t, "other"); err == nil {
		t.Fatalf("unknown key accepted")
	}
}

func TestEnvKeysAreAccepted(t *testing.T) {
	t.Setenv(EnvAPIKeys, " env-one ,, env-two")
	cfg := &sdkconfig.SDKConfig{}
	Register(cfg)
	t.Cleanup(func() { Register(nil) })
	for _, key := range []string{"env-one", "env-two"} {
		if err := authenticate(t, key); err != nil {
			t.Fatalf("key %q refused: %v", key, err)
		}
	}
	if err := authenticate(t, "other"); err == nil {
		t.Fatalf("unknown key accepted")
	}
	if len(cfg.APIKeys) != 0 {
		t.Fatalf("env keys leaked into the config: %v", cfg.APIKeys)
	}
}

func TestAFileKeyWhileTheVariableIsSetRefusesEverything(t *testing.T) {
	t.Setenv(EnvAPIKeys, "env-one")
	Register(&sdkconfig.SDKConfig{APIKeys: []string{"file-key"}})
	t.Cleanup(func() { Register(nil) })
	for _, key := range []string{"file-key", "env-one", "other"} {
		if err := authenticate(t, key); err == nil {
			t.Fatalf("key %q accepted although the config file holds a key", key)
		}
	}
}

func TestNoKeysAnywhere(t *testing.T) {
	os.Unsetenv(EnvAPIKeys)
	Register(&sdkconfig.SDKConfig{})
	t.Cleanup(func() { Register(nil) })
	registered := len(sdkaccess.RegisteredProviders()) > 0
	if !sealed {
		if registered {
			t.Fatalf("an ordinary build keeps upstream's open mode, but a provider is registered")
		}
		return
	}
	if !registered {
		t.Fatalf("sealed build: no provider registered, which allows every request")
	}
	if err := authenticate(t, "anything"); err == nil {
		t.Fatalf("sealed build: a key was accepted with none configured")
	}
	r := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	if _, err := sdkaccess.RegisteredProviders()[0].Authenticate(context.Background(), r); err == nil {
		t.Fatalf("sealed build: a request with no credential was accepted")
	}
}

func TestEnvKeysAloneRegisterTheProvider(t *testing.T) {
	t.Setenv(EnvAPIKeys, "only-env")
	Register(&sdkconfig.SDKConfig{})
	t.Cleanup(func() { Register(nil) })
	if err := authenticate(t, "only-env"); err != nil {
		t.Fatalf("env key refused with an empty config list: %v", err)
	}
}

func TestBlankEnvWithNoFileKeysRefusesEverything(t *testing.T) {
	for _, value := range []string{"", "  ", ",", " , ,"} {
		t.Setenv(EnvAPIKeys, value)
		Register(&sdkconfig.SDKConfig{})
		if len(sdkaccess.RegisteredProviders()) == 0 {
			t.Fatalf("value %q left no provider registered, which allows every request", value)
		}
		for _, key := range []string{"anything", "", " "} {
			if err := authenticate(t, key); err == nil {
				t.Fatalf("value %q: key %q accepted", value, key)
			}
		}
		r := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
		if _, err := sdkaccess.RegisteredProviders()[0].Authenticate(context.Background(), r); err == nil {
			t.Fatalf("value %q: a request with no credential was accepted", value)
		}
		Register(nil)
	}
}
