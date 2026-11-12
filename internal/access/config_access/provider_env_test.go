package configaccess

import (
	"context"
	"net/http"
	"net/http/httptest"
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

func TestEnvKeysAreAcceptedAlongsideConfigKeys(t *testing.T) {
	t.Setenv(EnvAPIKeys, " env-one ,, env-two")
	cfg := &sdkconfig.SDKConfig{APIKeys: []string{"file-key"}}
	Register(cfg)
	t.Cleanup(func() { Register(nil) })

	for _, key := range []string{"file-key", "env-one", "env-two"} {
		if err := authenticate(t, key); err != nil {
			t.Fatalf("key %q refused: %v", key, err)
		}
	}
	if err := authenticate(t, "other"); err == nil {
		t.Fatalf("unknown key accepted")
	}
	if len(cfg.APIKeys) != 1 || cfg.APIKeys[0] != "file-key" {
		t.Fatalf("env keys leaked into the config: %v", cfg.APIKeys)
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

func TestEmptyEnvAddsNoKey(t *testing.T) {
	t.Setenv(EnvAPIKeys, " , ,")
	Register(&sdkconfig.SDKConfig{APIKeys: []string{"file-key"}})
	t.Cleanup(func() { Register(nil) })
	if err := authenticate(t, ""); err == nil {
		t.Fatalf("empty key accepted")
	}
	if err := authenticate(t, " "); err == nil {
		t.Fatalf("blank key accepted")
	}
}
