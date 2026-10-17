#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Sign Mud Hut's verified-builds list (maintainer tool, never shipped in the binary).

  sign_verified_list.py keygen <dir>                 # writes <dir>/verified-list.key (0600) + .pub
  sign_verified_list.py sign <secret.key> <file>     # writes <file>.minisig
  sign_verified_list.py verify <public.pub> <file>   # checks <file>.minisig

The files are minisign's own formats (Ed25519; the signature is over the BLAKE2b-512 hash of the
file, "prehashed"; the secret key is stored unencrypted, like `minisign -G -W`), so the stock
`minisign` tool can sign and verify them too:
  minisign -S -s verified-list.key -m ledger/verified.json
  minisign -V -p ledger/verified.pub -m ledger/verified.json
Mud Hut verifies with the `minisign-verify` crate against the public key compiled in from
`ledger/verified.pub`. Keep the secret key OUTSIDE every repository.
Needs python3-cryptography.
"""
import base64, hashlib, os, sys, time

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey
from cryptography.hazmat.primitives import serialization


def raw_pub(pk):
    return pk.public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)


def keygen(d):
    os.makedirs(d, mode=0o700, exist_ok=True)
    os.chmod(d, 0o700)
    seed = os.urandom(32)
    key_id = os.urandom(8)
    sk = Ed25519PrivateKey.from_private_bytes(seed)
    pk = raw_pub(sk.public_key())
    sk64 = seed + pk
    chk = hashlib.blake2b(b"Ed" + key_id + sk64, digest_size=32).digest()
    blob = b"Ed" + b"\0\0" + b"B2" + b"\0" * 32 + b"\0" * 8 + b"\0" * 8 + key_id + sk64 + chk
    kid = key_id[::-1].hex().upper()
    keyp = os.path.join(d, "verified-list.key")
    fd = os.open(keyp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write("untrusted comment: minisign encrypted secret key\n")
        f.write(base64.b64encode(blob).decode() + "\n")
    pubp = os.path.join(d, "verified-list.pub")
    with open(pubp, "w") as f:
        f.write(f"untrusted comment: minisign public key {kid}\n")
        f.write(base64.b64encode(b"Ed" + key_id + pk).decode() + "\n")
    print(keyp)
    print(pubp)


def load_sk(path):
    lines = open(path).read().splitlines()
    blob = base64.b64decode(lines[1])
    if blob[0:2] != b"Ed" or blob[2:4] != b"\0\0":
        sys.exit("only unencrypted minisign Ed25519 secret keys are supported (minisign -G -W)")
    key_id = blob[54:62]
    sk64 = blob[62:126]
    chk = blob[126:158]
    if hashlib.blake2b(b"Ed" + key_id + sk64, digest_size=32).digest() != chk:
        sys.exit("secret key checksum mismatch")
    return key_id, Ed25519PrivateKey.from_private_bytes(sk64[:32])


def sign(keyp, path):
    key_id, sk = load_sk(keyp)
    data = open(path, "rb").read()
    sig = sk.sign(hashlib.blake2b(data, digest_size=64).digest())
    trusted = "timestamp:%d\tfile:%s\thashed" % (int(time.time()), os.path.basename(path))
    gsig = sk.sign(sig + trusted.encode())
    out = path + ".minisig"
    with open(out, "w") as f:
        f.write("untrusted comment: signature from minisign secret key\n")
        f.write(base64.b64encode(b"ED" + key_id + sig).decode() + "\n")
        f.write("trusted comment: " + trusted + "\n")
        f.write(base64.b64encode(gsig).decode() + "\n")
    print(out)


def verify(pubp, path):
    pubb = base64.b64decode(open(pubp).read().splitlines()[1])
    key_id, pk = pubb[2:10], Ed25519PublicKey.from_public_bytes(pubb[10:42])
    lines = open(path + ".minisig").read().splitlines()
    s = base64.b64decode(lines[1])
    if s[2:10] != key_id:
        sys.exit("signed by a different key")
    data = open(path, "rb").read()
    pk.verify(s[10:74], hashlib.blake2b(data, digest_size=64).digest() if s[0:2] == b"ED" else data)
    pk.verify(base64.b64decode(lines[3]), s[10:74] + lines[2][len("trusted comment: "):].encode())
    print("signature valid:", lines[2])


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "keygen" and len(sys.argv) == 3:
        keygen(sys.argv[2])
    elif cmd == "sign" and len(sys.argv) == 4:
        sign(sys.argv[2], sys.argv[3])
    elif cmd == "verify" and len(sys.argv) == 4:
        verify(sys.argv[2], sys.argv[3])
    else:
        sys.exit(__doc__)
