"""The stand-in platform's registry for `make e2e-plugin-area` (contract v25;
spec/plugins-are-shared-through-registries, "What the tests prove"), made
once and kept beside this file: run again only to change what it holds.

    docker run --rm -v "$PWD/e2e/plugin-area/registry:/out" python:3.12-slim \
        sh -c 'pip -q install cryptography && python /out/make.py /out'

It signs, as the platform signs (registry_signing.py: ECDSA P-256 over DSSE's
pre-authentication encoding of canonical JSON), with a key made here and
thrown away; only its public half is kept, as `platform-key.pem`, which the
harness's conductor is given as the chart's platform key. What it writes:

- `catalogue.json`: PullCatalogueReply as the platform answers it -- three
  listings and the signing keys' status. `open-meridian/stand-in` lists
  1.3.0, pulled for a terms breach; 1.4.0 and 1.5.0, whose overview
  differs and whose troubleshooting mentions a jailbreak (the screen's
  "prompt talk", withheld where an agent reads it); and 1.6.0, whose layer
  the stand-in serves altered. `other-org/revoked` lists 1.0.0, built after
  its organisation's key was revoked. From v28 (W8.18, W8.19)
  `open-meridian/stand-in-rates` lists 0.1.0, which installs cleanly, and
  the catalogue carries one starter pack, `open-meridian/stand-in-desk`:
  stand-in-rates and revoked preselected under "Data", the stand-in to
  choose under "Brokers".
- `resolutions.json`: each version's DownloadListedVersionReply, its
  listing statement among it, to be pulled from the stand-in's own OCI
  registry (`platform:8443`, over TLS).
- `oci/`: every manifest and blob by its digest's hex, each image's
  referrers (the provenance and the image's metadata, DSSE envelopes) and
  `index.json`: each manifest's media type and each image's referrers.
  From v26 (found on the development deployment 2026-10-18) 1.3.0 and
  1.4.0 carry bare envelopes, listed by the referrers API, as cosign's
  `.att` did; 1.5.0, 1.6.0 and other-org's 1.0.0 carry them as the
  platform's builder now does, each in a Sigstore bundle (cosign 2.5's
  `--new-bundle-format`: an empty config, one layer holding the bundle,
  its verificationMaterial naming a key that is not the platform's, which
  core must never read), listed only under the referrers tag schema
  (`sha256-<hex>`), the referrers API answering 404, as ghcr.io does.
- `cases.json`: what the run names -- each version's digest, and 1.4.0's
  provenance referrer, which the run removes from the deployment's own
  registry after install to show the launch's verification again refuse.
"""
import base64
import hashlib
import json
import os
import sys

from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec

OUT = sys.argv[1]
KEY = ec.generate_private_key(ec.SECP256R1())
IN_TOTO = "application/vnd.in-toto+json"
KEY_STATUS = "application/vnd.open-meridian.signing-key-status+json"
LISTING = "https://open-meridian.dev/attestation/plugin-listing/v1"
PROVENANCE = "https://slsa.dev/provenance/v1"
METADATA = "https://open-meridian.dev/attestation/plugin-metadata/v1"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
INDEX = "application/vnd.oci.image.index.v1+json"
DSSE = "application/vnd.dsse.envelope.v1+json"
BUNDLE = "application/vnd.dev.sigstore.bundle.v0.3+json"
EMPTY = "application/vnd.oci.empty.v1+json"
OM_KEY = "SHA256:e2e-open-meridian-release"
OTHER_KEY = "SHA256:e2e-other-org-release"
NS = 1_000_000_000
LISTED_AT = "2026-10-16T12:00:00Z"
LISTED_AT_NS = 1_792_152_000 * NS


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def pae(payload_type, body):
    return b"DSSEv1 %d %s %d %s" % (len(payload_type), payload_type.encode(), len(body), body)


def sign(message):
    return base64.b64encode(KEY.sign(message, ec.ECDSA(hashes.SHA256()))).decode()


def envelope(payload_type, document):
    body = canonical(document).encode()
    return {"payloadType": payload_type, "payload": base64.b64encode(body).decode(),
            "signatures": [{"keyid": "e2e-platform", "sig": sign(pae(payload_type, body))}]}


def statement(predicate_type, digest, predicate):
    return envelope(IN_TOTO, {
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"name": "platform:8443/plugins/x", "digest": {"sha256": digest.split(":")[1]}}],
        "predicateType": predicate_type, "predicate": predicate})


def digest_of(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


BLOBS = {}
MEDIA = {}
REFERRERS = {}
# Images whose referrers only the tag schema lists (v26): the API 404s.
TAG_SCHEMA_ONLY = []


def put(data, media=None):
    digest = digest_of(data)
    BLOBS[digest] = data
    if media:
        MEDIA[digest] = media
    return digest


def descriptor(data, media):
    return {"mediaType": media, "digest": put(data), "size": len(data)}


def texts(version):
    overview = ("# Stand-in\n\nReads the holdings of the stand-in's accounts.\n" if version != "1.5.0"
                else "# Stand-in\n\nReads the holdings and the activity of the stand-in's accounts.\n")
    return [("overview", overview),
            ("setup", "# Setup\n\n## Enter the key\n\nEnter the stand-in's key at the Settings form.\n"),
            ("troubleshooting", "# Troubleshooting\n\nIf a jailbreak warning appears in the "
                                "stand-in's log, the key was rotated: enter the new one.\n")]


def metadata(name, version):
    return {"name": name, "version": version, "roles": ["custody"], "interface": False,
            "sdk_version": "0.29.0",
            "declaration": {
                "secret_settings": ["api_key"], "not_carried": [], "storage": None,
                "settings": [{"name": "region", "label": "Region", "type": "SETTING_TYPE_STRING",
                              "description": "Where the stand-in's accounts are held."}],
                "documentation": [{"name": section,
                                   "digest": digest_of(text.encode()),
                                   "bytes": len(text.encode())}
                                  for section, text in texts(version)]}}


def image(listing, version, signer, tampered=False, bundled=False):
    """An image and its referrers, as the builder pushes them: bare DSSE
    envelopes, or (`bundled`) each in a Sigstore bundle listed under the
    referrers tag schema alone."""
    layer = f"the stand-in, {listing} {version}\n".encode()
    config = canonical({"architecture": "amd64", "os": "linux",
                        "rootfs": {"type": "layers", "diff_ids": [digest_of(layer)]}}).encode()
    manifest = canonical({"schemaVersion": 2, "mediaType": MANIFEST,
                          "config": descriptor(config, "application/vnd.oci.image.config.v1+json"),
                          "layers": [descriptor(layer, "application/vnd.oci.image.layer.v1.tar")]}).encode()
    digest = put(manifest, MANIFEST)
    if tampered:
        # Served altered: the bytes under the layer's digest are not it.
        BLOBS[digest_of(layer)] = layer.replace(b"stand-in", b"stand_in")
    name = listing.split("/")[1]
    referrers = []
    for predicate_type, predicate in [
        (PROVENANCE, {"repository": f"{listing.split('/')[0]}/plugin-{name}", "tag": f"v{version}",
                      "commit": "c" * 40, "signer": signer}),
        (METADATA, {"metadata": metadata(name, version), "schema_version": "v25",
                    "documentation": [{"name": s, "text": t} for s, t in texts(version)]}),
    ]:
        signed_statement = statement(predicate_type, digest, predicate)
        kind = BUNDLE if bundled else DSSE
        if bundled:
            held = canonical({"mediaType": BUNDLE, "dsseEnvelope": signed_statement,
                              "verificationMaterial": {"publicKey": {"hint": "not-the-platforms"}}}).encode()
        else:
            held = canonical(signed_statement).encode()
        body = {"schemaVersion": 2, "mediaType": MANIFEST, "artifactType": kind,
                "config": descriptor(b"{}", EMPTY),
                "layers": [descriptor(held, kind)],
                "subject": {"mediaType": MANIFEST, "digest": digest, "size": len(manifest)}}
        if bundled:
            body["annotations"] = {"dev.sigstore.bundle.content": "dsse-envelope",
                                   "dev.sigstore.bundle.predicateType": predicate_type}
        referrer = canonical(body).encode()
        referrers.append({"mediaType": MANIFEST,
                          "artifactType": EMPTY if bundled else DSSE,
                          "digest": put(referrer, MANIFEST), "size": len(referrer),
                          "predicateType": predicate_type})
    REFERRERS[digest] = {"schemaVersion": 2, "mediaType": INDEX,
                         "manifests": [{k: v for k, v in r.items() if k != "predicateType"}
                                       for r in referrers]}
    if bundled:
        TAG_SCHEMA_ONLY.append(digest)
    return digest, referrers


def listed(listing, version, signer, pull=None, tampered=False, bundled=False):
    digest, referrers = image(listing, version, signer, tampered, bundled)
    name = listing.split("/")[1]
    signed = canonical(statement(LISTING, digest, {
        "listing": listing, "version": version, "registry": "community",
        "signer_key": signer, "listed_at": LISTED_AT, "roles": ["custody"],
        "approved_roles": [], "floor": "v25", "review_id": ""}))
    entry = {"listing": listing, "version": version, "registry": "community",
             "image_digest": digest, "metadata": metadata(name, version), "approved_roles": [],
             "schema_version": "v25", "build": {"signer_fingerprint": signer},
             "listed_at_ns": LISTED_AT_NS, "withdrawn_at_ns": 0, "sequence": 4,
             "listing_statement": signed}
    if pull:
        entry["pull"] = pull
    resolution = {"image_digest": digest, "pull_from": f"platform:8443/plugins/{listing}",
                  "pull_token": "", "pull_token_expires_at_ns": 0, "listing_statement": signed}
    return entry, resolution, referrers


def listing_of(name, versions):
    return {"listing": name, "repository": f"https://github.com/{name.split('/')[0]}/plugin-{name.split('/')[1]}",
            "versions": versions, "regions": ["US"], "asset_classes": ["equity"],
            "support_contact": "", "stars": 3, "installs": 0, "instances": 0, "counts_shown": False}


def main():
    resolutions, cases, stand_in = {}, {}, []
    pulled = {"listing": "open-meridian/stand-in", "version": "1.3.0",
              "kind": "PULL_REASON_KIND_TERMS_BREACH",
              "reason": "It sent account numbers to a host it did not declare.",
              "pulled_at_ns": LISTED_AT_NS + 3600 * NS, "notice_id": "", "restored_at_ns": 0}
    for version, pull, tampered in [("1.3.0", pulled, False), ("1.4.0", None, False),
                                    ("1.5.0", None, False), ("1.6.0", None, True)]:
        entry, resolution, referrers = listed("open-meridian/stand-in", version, OM_KEY, pull,
                                              tampered, bundled=version in ("1.5.0", "1.6.0"))
        stand_in.append(entry)
        resolutions[f"open-meridian/stand-in@{version}"] = resolution
        cases[version] = entry["image_digest"]
        if version == "1.4.0":
            cases["provenance_1.4.0"] = next(r["digest"] for r in referrers
                                             if r["predicateType"] == PROVENANCE)
    revoked, resolution, _ = listed("other-org/revoked", "1.0.0", OTHER_KEY, bundled=True)
    resolutions["other-org/revoked@1.0.0"] = resolution
    cases["revoked"] = revoked["image_digest"]
    # v28: a listing a starter pack installs cleanly.
    rates, resolution, _ = listed("open-meridian/stand-in-rates", "0.1.0", OM_KEY)
    resolutions["open-meridian/stand-in-rates@0.1.0"] = resolution
    cases["stand-in-rates"] = rates["image_digest"]
    key = {"registered_by_person_id": "", "revoked_by_person_id": "", "revoke_reason": "",
           "revoked_at_ns": 0, "not_trusted_after_ns": 0, "public_key": "ssh-ed25519 AAAAe2e"}
    status = {"issued_at_ns": LISTED_AT_NS + 7200 * NS, "keys": [
        dict(key, key_id="SGK-om-1", organisation="open-meridian", name="release",
             fingerprint=OM_KEY, registered_at_ns=LISTED_AT_NS - 86400 * NS),
        dict(key, key_id="SGK-oo-1", organisation="other-org", name="release",
             fingerprint=OTHER_KEY, registered_at_ns=LISTED_AT_NS - 86400 * NS,
             revoked_at_ns=LISTED_AT_NS - 3600 * NS, revoked_by_person_id="PER-oo",
             revoke_reason="The laptop holding it was lost.",
             not_trusted_after_ns=LISTED_AT_NS - 3600 * NS)]}
    status["signature"] = sign(pae(KEY_STATUS, canonical(status).encode()))
    member = lambda listing, selection, group, floor: {
        "listing": listing, "registry": "community", "selection": f"PACK_SELECTION_{selection}",
        "group": group, "minimum_version": floor}
    desk = {"pack": "open-meridian/stand-in-desk", "curator": "Open Meridian", "sequence": 4,
            "withdrawn_at_ns": 0,
            "revision": {"revision": 1, "title": "Stand-in desk",
                         "description": "The stand-in's rates and accounts, for the harness.",
                         "asset_classes": ["equity"], "note": "Reviewed.",
                         "published_at_ns": LISTED_AT_NS,
                         "members": [member("open-meridian/stand-in-rates", "PRESELECTED", "Data", "0.1.0"),
                                     member("other-org/revoked", "PRESELECTED", "Data", "1.0.0"),
                                     member("open-meridian/stand-in", "CHOOSE", "Brokers", "1.4.0")]}}
    catalogue = {"listings": [listing_of("open-meridian/stand-in", stand_in),
                              listing_of("open-meridian/stand-in-rates", [rates]),
                              listing_of("other-org/revoked", [revoked])],
                 "packs": [desk], "sequence": 4, "key_status": status}

    os.makedirs(f"{OUT}/oci", exist_ok=True)
    for digest, data in BLOBS.items():
        with open(f"{OUT}/oci/{digest.split(':')[1]}", "wb") as kept:
            kept.write(data)
    for name, value in [("catalogue.json", catalogue), ("resolutions.json", resolutions),
                        ("cases.json", cases),
                        ("oci/index.json", {"media": MEDIA, "referrers": REFERRERS,
                                            "tag_schema_only": TAG_SCHEMA_ONLY})]:
        with open(f"{OUT}/{name}", "w") as kept:
            json.dump(value, kept, indent=1, sort_keys=True)
            kept.write("\n")
    with open(f"{OUT}/platform-key.pem", "wb") as kept:
        kept.write(KEY.public_key().public_bytes(
            serialization.Encoding.PEM, serialization.PublicFormat.SubjectPublicKeyInfo))


main()
