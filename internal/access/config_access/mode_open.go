//go:build !sealed

package configaccess

// sealed is false in an ordinary build: with no client keys the provider is unregistered and, as upstream
// documents, every request is allowed.
const sealed = false
