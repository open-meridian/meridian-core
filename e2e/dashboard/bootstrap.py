"""Configure the bundled Zitadel for the dashboard, idempotently.

What the chart's post-install Job will do (spec, ruling 14), done here for the
end-to-end test: every object is looked up by name first and made only when
missing, so a second run changes nothing and reports the same values.

Writes onto the bootstrap volume:
  client-id                  the dashboard's OIDC client
  project-id                 the project whose roles count as groups
  hook-intent-signing-key    the /intent target's signing key
  hook-token-signing-key     the /token target's signing key
  ldap-idp-id, org-id        for the runner
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


def project():
    found = call("POST", "/management/v1/projects/_search",
                 {"queries": [{"nameQuery": equals("name", PROJECT)}]}).get("result", [])
    pid = found[0]["id"] if found else call(
        "POST", "/management/v1/projects", {"name": PROJECT, "projectRoleAssertion": True})["id"]
    for role in ROLES:
        tolerate_exists("POST", f"/management/v1/projects/{pid}/roles", {"roleKey": role, "displayName": role})
    return pid


def app(pid):
    found = call("POST", f"/management/v1/projects/{pid}/apps/_search",
                 {"queries": [{"nameQuery": equals("name", APP)}]}).get("result", [])
    if found:
        return found[0]["oidcConfig"]["clientId"]
    # A public client: the code flow with PKCE, which the dashboard always
    # uses, and no secret to hand around. devMode only because the redirect
    # is plain http on a name that is not localhost. The userinfo assertion
    # puts the hook's `groups` claim in the ID token, which is where the
    # dashboard reads it; auth_time is in every ID token Zitadel issues.
    made = call("POST", f"/management/v1/projects/{pid}/apps/oidc", {
        "name": APP,
        "redirectUris": [REDIRECT],
        "responseTypes": ["OIDC_RESPONSE_TYPE_CODE"],
        "grantTypes": ["OIDC_GRANT_TYPE_AUTHORIZATION_CODE"],
        "appType": "OIDC_APP_TYPE_WEB",
        "authMethodType": "OIDC_AUTH_METHOD_TYPE_NONE",
        "devMode": True,
        "accessTokenType": "OIDC_TOKEN_TYPE_BEARER",
        "idTokenRoleAssertion": True,
        "idTokenUserinfoAssertion": True,
        "accessTokenRoleAssertion": False,
    })
    return made["clientId"]


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
    body = {
        "name": LDAP_IDP,
        "servers": ["ldap://ldap:1389"],
        "startTls": False,
        "baseDn": "ou=people,dc=example,dc=org",
        "bindDn": "cn=admin,dc=example,dc=org",
        "bindPassword": "ldap-admin-dev-only",
        "userBase": "dn",
        "userObjectClasses": ["inetOrgPerson"],
        "userFilters": ["uid"],
        "timeout": "10s",
        # memberOf mapped onto an otherwise unused slot, because Zitadel asks
        # the directory only for mapped attributes and would never see it.
        "attributes": {"idAttribute": "uid", "firstNameAttribute": "givenName", "lastNameAttribute": "sn",
                       "displayNameAttribute": "cn", "preferredUsernameAttribute": "uid",
                       "emailAttribute": "mail", "profileAttribute": "memberOf"},
        # Auto-update rewrites the person on every sign-in, which is what
        # carries a removed group away; auto-creation makes a first sign-in work.
        "providerOptions": {"isLinkingAllowed": True, "isCreationAllowed": True,
                            "isAutoCreation": True, "isAutoUpdate": True},
    }
    found = call("POST", "/admin/v1/idps/templates/_search",
                 {"queries": [{"idpNameQuery": equals("name", LDAP_IDP)}]}).get("result", [])
    if found:
        idp = found[0]["id"]
    else:
        idp = call("POST", "/admin/v1/idps/ldap", body)["id"]
    tolerate_exists("POST", "/admin/v1/policies/login/idps", {"idpId": idp})
    return idp


def target(name, path):
    """A webhook target, and its signing key. Zitadel reads the key back on
    search, so an existing target is reused rather than re-keyed."""
    found = [t for t in call("POST", "/v2/actions/targets/search", {}).get("targets", []) if t["name"] == name]
    if found:
        return found[0]["id"], found[0]["signingKey"]
    made = call("POST", "/v2/actions/targets", {
        "name": name, "endpoint": HOOK + path, "timeout": "10s",
        # A failing hook fails the sign-in, rather than letting it through
        # with no groups, or with the groups it had last time.
        "restCall": {"interruptOnError": True},
    })
    return made["id"], made["signingKey"]


def executions(intent_target, token_target):
    call("PUT", "/v2/actions/executions",
         {"condition": {"response": {"method": INTENT_METHOD}}, "targets": [intent_target]})
    for function in ("preaccesstoken", "preuserinfo"):
        call("PUT", "/v2/actions/executions",
             {"condition": {"function": {"name": function}}, "targets": [token_target]})
    present = call("POST", "/v2/actions/executions/search", {}).get("executions", [])
    return present


def main():
    wait_ready()
    org_id = call("GET", "/management/v1/orgs/me")["org"]["id"]
    pid = project()
    client_id = app(pid)
    people = {name: person(org_id, pid, name, roles) for name, roles in PEOPLE}
    idp = ldap_provider()
    intent_target, intent_key = target("meridian-group-hook-intent", "/intent")
    token_target, token_key = target("meridian-group-hook-token", "/token")
    present = executions(intent_target, token_target)

    write("org-id", org_id)
    write("project-id", pid)
    write("client-id", client_id)
    write("ldap-idp-id", idp)
    write("hook-intent-signing-key", intent_key)
    write("hook-token-signing-key", token_key)
    print(f"bootstrap: org {org_id}, project {pid}, client {client_id}, LDAP provider {idp}")
    print(f"bootstrap: people {people}")
    print(f"bootstrap: targets intent={intent_target} token={token_target}; "
          f"{len(present)} executions: {[e.get('condition') for e in present]}")


if __name__ == "__main__":
    main()
