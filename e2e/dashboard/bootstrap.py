"""What only the end-to-end test needs, after the chart's setup has run.

`meridian-group-hook setup` -- the chart's setup Job -- has already made the
project, the dashboard's client, the LDAP provider, and the hook's targets and
executions, and written project-id, client-id and the two signing keys onto
the bootstrap volume. This adds the test's people, with their roles, and
writes org-id and ldap-idp-id for the runner. Idempotent.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from zt import ApiError, BOOTSTRAP, call, wait_ready  # noqa: E402

PROJECT = "meridian-dashboard"
APP = "meridian-dashboard"
REDIRECT = os.environ.get("DASHBOARD_REDIRECT", "http://dashboard:8080/callback")
HOOK = os.environ.get("GROUP_HOOK", "http://group-hook:8090")
ROLES = ["e2e-staff"]
# Zitadel-native people: (username, roles on the dashboard's project).
PEOPLE = [("ada", ["e2e-staff"]), ("cy", ["e2e-staff"])]
PASSWORD = "Password1!"
LDAP_IDP = "Firm LDAP"
INTENT_METHOD = "/zitadel.user.v2.UserService/RetrieveIdentityProviderIntent"


def equals(field, value):
    return {field: value, "method": "TEXT_QUERY_METHOD_EQUALS"}


def write(name, value):
    path = os.path.join(BOOTSTRAP, name)
    with open(path + ".tmp", "w") as f:
        f.write(value)
    os.replace(path + ".tmp", path)


def tolerate_exists(method, path, body):
    """Make it; an object that already exists is the same outcome."""
    try:
        return call(method, path, body)
    except ApiError as failed:
        if failed.status == 409 or "AlreadyExists" in failed.body:
            return None
        raise


def person(org_id, pid, username, roles):
    found = call("POST", "/v2/users", {"queries": [{"userNameQuery": {
        "userName": username, "method": "TEXT_QUERY_METHOD_EQUALS"}}]}).get("result", [])
    if found:
        uid = found[0]["userId"]
    else:
        uid = call("POST", "/v2/users/human", {
            "username": username, "organization": {"orgId": org_id},
            "profile": {"givenName": username.capitalize(), "familyName": "Native"},
            "email": {"email": f"{username}@native.example.org", "isVerified": True},
            "password": {"password": PASSWORD, "changeRequired": False},
        })["userId"]
    grants = call("POST", "/management/v1/users/grants/_search",
                  {"queries": [{"userIdQuery": {"userId": uid}}, {"projectIdQuery": {"projectId": pid}}]}
                  ).get("result", [])
    if not grants:
        call("POST", f"/management/v1/users/{uid}/grants", {"projectId": pid, "roleKeys": roles})
    return uid


def ldap_provider():
    """The LDAP provider setup made, by the name it was given."""
    found = call("POST", "/admin/v1/idps/templates/_search",
                 {"queries": [{"idpNameQuery": equals("name", LDAP_IDP)}]}).get("result", [])
    if not found:
        raise RuntimeError(f"setup made no provider named {LDAP_IDP}")
    return found[0]["id"]


def read(name):
    with open(os.path.join(BOOTSTRAP, name)) as f:
        return f.read().strip()


def main():
    wait_ready()
    org_id = call("GET", "/management/v1/orgs/me")["org"]["id"]
    pid = read("project-id")
    people = {name: person(org_id, pid, name, roles) for name, roles in PEOPLE}
    idp = ldap_provider()
    write("org-id", org_id)
    write("ldap-idp-id", idp)
    print(f"bootstrap: org {org_id}, project {pid}, LDAP provider {idp}, people {people}")


if __name__ == "__main__":
    main()
