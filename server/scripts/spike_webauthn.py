"""Spike (c): a software authenticator registers through py_webauthn (exactly as auth/passkeys.py
stores it), then answers a login challenge from webauthn-rs (examples/spike_webauthn.rs).

    uv run --with webauthn==3.0.1 --with cbor2==6.2.1 --with cryptography==50.0.2 python server/scripts/spike_webauthn.py server/target/debug/examples/spike_webauthn
"""

import base64
import hashlib
import json
import struct
import subprocess
import sys
import uuid

import cbor2
import webauthn
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec
from webauthn.helpers.structs import AuthenticatorSelectionCriteria, ResidentKeyRequirement, UserVerificationRequirement

RP_ID, ORIGIN = "localhost", "http://localhost:8080"


def b64(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def unb64(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


key = ec.generate_private_key(ec.SECP256R1())
nums = key.public_key().public_numbers()
cose = cbor2.dumps({1: 2, 3: -7, -1: 1, -2: nums.x.to_bytes(32, "big"), -3: nums.y.to_bytes(32, "big")})
cred_id = b"synthetic-rivera-passkey-0001"
user_id = uuid.uuid4()
rp_hash = hashlib.sha256(RP_ID.encode()).digest()

# --- registration with py_webauthn, as begin_registration / verify_registration do
opts = webauthn.generate_registration_options(
    rp_id=RP_ID,
    rp_name="OwnHealth",
    user_id=user_id.bytes,
    user_name="alex",
    authenticator_selection=AuthenticatorSelectionCriteria(
        resident_key=ResidentKeyRequirement.REQUIRED, user_verification=UserVerificationRequirement.REQUIRED
    ),
)
cdata = json.dumps({"type": "webauthn.create", "challenge": b64(opts.challenge), "origin": ORIGIN}).encode()
auth_data = rp_hash + bytes([0x45]) + struct.pack(">I", 0) + bytes(16) + struct.pack(">H", len(cred_id)) + cred_id + cose
att = cbor2.dumps({"fmt": "none", "attStmt": {}, "authData": auth_data})
reg = webauthn.verify_registration_response(
    credential={
        "id": b64(cred_id),
        "rawId": b64(cred_id),
        "response": {"clientDataJSON": b64(cdata), "attestationObject": b64(att), "transports": ["internal"]},
        "type": "public-key",
    },
    expected_challenge=opts.challenge,
    expected_rp_id=RP_ID,
    expected_origin=ORIGIN,
    require_user_verification=True,
)
# what auth/passkeys.py writes into passkey (id, public_key, sign_count)
row = {"cred_id": b64(reg.credential_id), "public_key_hex": reg.credential_public_key.hex(), "sign_count": reg.sign_count}


def assertion(challenge: str, counter: int, tamper: bool = False) -> dict:
    cd = json.dumps({"type": "webauthn.get", "challenge": challenge, "origin": ORIGIN}).encode()
    ad = rp_hash + bytes([0x05]) + struct.pack(">I", counter)
    sig = key.sign(ad + hashlib.sha256(cd).digest(), ec.ECDSA(hashes.SHA256()))
    if tamper:
        sig = sig[:-1] + bytes([sig[-1] ^ 1])
    return {
        "id": b64(cred_id),
        "rawId": b64(cred_id),
        "response": {
            "authenticatorData": b64(ad),
            "clientDataJSON": b64(cd),
            "signature": b64(sig),
            "userHandle": b64(user_id.bytes),
        },
        "type": "public-key",
        "extensions": {},
    }


for tamper in (False, True):
    p = subprocess.Popen([sys.argv[1]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    p.stdin.write(json.dumps(row) + "\n")
    p.stdin.flush()
    options = json.loads(p.stdout.readline())
    a = assertion(options["publicKey"]["challenge"], 1, tamper)
    p.stdin.write(json.dumps(a) + "\n")
    p.stdin.flush()
    result = json.loads(p.stdout.readline())
    p.wait()
    print("tampered" if tamper else "genuine", "->", result, "| user", user_id)
