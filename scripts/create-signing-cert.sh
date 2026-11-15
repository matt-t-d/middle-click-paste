#!/bin/sh
# Create a self-signed code-signing identity in the login keychain.
#
# `middle-click-paste install` signs the installed binary with it, so macOS ties
# the Accessibility grant to this certificate instead of the build's content
# hash, and the grant survives reinstalls of new builds.
set -eu

NAME="middle-click-paste dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-certificate -c "$NAME" "$KEYCHAIN" >/dev/null 2>&1; then
    echo "'$NAME' already exists in the login keychain."
    exit 0
fi

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
pass=$(openssl rand -hex 16)

cat > "$dir/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -config "$dir/cert.cnf" -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
# The legacy algorithms are the only ones `security import` understands.
openssl pkcs12 -export -inkey "$dir/key.pem" -in "$dir/cert.pem" \
    -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
    -name "$NAME" -passout "pass:$pass" -out "$dir/id.p12" 2>/dev/null

# -T lets codesign use the key without a keychain prompt.
security import "$dir/id.p12" -k "$KEYCHAIN" -P "$pass" -T /usr/bin/codesign >/dev/null

echo "Created '$NAME' in the login keychain."
