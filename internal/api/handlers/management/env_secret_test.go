package management

import "testing"

func TestEnvSecretNeverForcesRemoteForTheLocalKey(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "")
	t.Setenv(EnvManagementKey, "  local-key ")
	secret, remote := EnvSecret()
	if secret != "local-key" || remote {
		t.Fatalf("got %q remote=%v, want the key with remote off", secret, remote)
	}
}

func TestEnvSecretLocalKeyWinsOverThePassword(t *testing.T) {
	t.Setenv("MANAGEMENT_PASSWORD", "remote-pass")
	t.Setenv(EnvManagementKey, "local-key")
	secret, remote := EnvSecret()
	if secret != "local-key" || remote {
		t.Fatalf("got %q remote=%v, want the local key with remote off", secret, remote)
	}
}

func TestEnvSecretPasswordKeepsItsOldMeaning(t *testing.T) {
	t.Setenv(EnvManagementKey, " ")
	t.Setenv("MANAGEMENT_PASSWORD", "remote-pass")
	secret, remote := EnvSecret()
	if secret != "remote-pass" || !remote {
		t.Fatalf("got %q remote=%v, want the password with remote on", secret, remote)
	}
	t.Setenv("MANAGEMENT_PASSWORD", "")
	if secret, remote := EnvSecret(); secret != "" || remote {
		t.Fatalf("got %q remote=%v, want nothing", secret, remote)
	}
}
