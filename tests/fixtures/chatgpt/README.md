# ChatGPT fixtures

`test-only-rsa.pk8` is a synthetic RSA signing key used exclusively by local authentication tests. It has no relationship to OpenAI, a user account or production credentials. `test-only-jwks.json` is its matching public key. TLS certificates and OAuth credentials are generated or invented by the fixture tests; no live sign-in is exercised.
