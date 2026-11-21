#!/bin/sh
# One code-signing identity for Silli's local builds (docs/silli-stick.md section 9). macOS keeps
# Screen Recording and Accessibility consent for an app by its signature; an app signed ad hoc is
# a new app each build, so the consent was lost at every rebuild. Signed with this certificate,
# Silli is the same app to macOS from build to build. Run once per Mac; macOS asks for your
# password to trust the certificate for code signing. Nothing leaves this Mac.
#
# After running it, remove any "Silli" rows in System Settings → Privacy & Security once (they
# belong to the builds signed ad hoc); from then on rebuilds keep the consent.
set -eu
name="Silli local development"
keychain="$HOME/Library/Keychains/login.keychain-db"
if security find-identity -v -p codesigning | grep -q "\"$name\""; then
    echo "\"$name\" is already in your login keychain."
    exit 0
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/cert.conf" <<CONF
[req]
distinguished_name = dn
prompt = no
x509_extensions = ext
[dn]
CN = $name
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
CONF
# a ten-year self-signed certificate for code signing, its key never written outside $work
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -config "$work/cert.conf" \
    -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null
pass=$(/usr/bin/openssl rand -hex 16)
/usr/bin/openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" -name "$name" \
    -out "$work/identity.p12" -passout "pass:$pass"
# codesign may use the key without asking each time
security import "$work/identity.p12" -k "$keychain" -P "$pass" -T /usr/bin/codesign >/dev/null
echo "Trusting the certificate for code signing (macOS asks for your password)..."
security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$work/cert.pem"
security find-identity -v -p codesigning | grep "\"$name\"" \
    || { echo "The identity is not usable for code signing." >&2; exit 1; }
echo "Done. packaging/mac/app.sh and run-gui.sh sign with it from now on."
