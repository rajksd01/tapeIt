#!/bin/zsh
# One-time: creates a local self-signed code-signing identity so macOS
# privacy permissions (Screen Recording, Camera, Mic) survive rebuilds.
set -euo pipefail

NAME="ScreenPen Local Signing"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

if security find-identity -p codesigning | grep -q "$NAME"; then
    echo "Already installed: $NAME"
    exit 0
fi

cat > "$WORK/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:false
EOF

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$WORK/key.pem" -out "$WORK/cert.pem" -config "$WORK/cert.cnf"
openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
    -out "$WORK/id.p12" -passout pass:screenpen
security import "$WORK/id.p12" -k ~/Library/Keychains/login.keychain-db \
    -P screenpen -T /usr/bin/codesign

security find-identity -p codesigning | grep "$NAME"
