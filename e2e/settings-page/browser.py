"""A plugin's Settings pages in a real browser (make e2e-settings-page).

Run against the dashboard's own pages for a SnapTrade-shaped instance, served
by crates/dashboard/src/admin/tests/served.rs with the kit the image carries. The
product owner, 2026-10-05, of SnapTrade 0.11.0's Settings: "why does this
screen force scrolling again and do we assume only 4 plan-code links will be
needed?" So, in headless Chromium:

- the kit's entry grid upgrades on a table setting's page (the dashboard loads
  the component), adds rows past four, and posts them; the page then holds
  them, each stamped by the stand-in conductor;
- a table of 200 rows, the most it declares, is accepted, the grid offers no
  201st, and 201 posted anyway are refused, naming the most;
- without script the plain table still posts, the rows held and a few blank;
- every page -- Settings and each of its groups' tabs, and each table's tab,
  in the plugin's area and in the admin portal -- fits one screen at 1440x900
  and 390x844 by the kit's own overflow check (lib/fit.js, fitProblems).

PASS=fit checks the fit alone (the development deployment's run, whose
Developer group and banner the ordinary run has not). Prints one line per
check, writes a screenshot of each page and size to /out, and exits non-zero
if any check failed.
"""

import os
import re
import sys
from urllib.parse import urlencode

from playwright.sync_api import sync_playwright

BASE = os.environ.get("E2E_DASHBOARD", "http://settings-page:8080").rstrip("/")
SESSION = os.environ["E2E_SESSION"]
PASS = os.environ.get("PASS", "all")
OUT = os.environ.get("OUT", "/out")
INSTANCE = "snaptrade-1"
SIZES = [("desktop", 1440, 900), ("phone", 390, 844)]
AREA = f"{BASE}/plugins/{INSTANCE}?level=admin&tab="
PORTAL = f"{BASE}/admin/plugins/{INSTANCE}?tab="
PLAN = "setting-plan_code_links"
CASH = "setting-counted_as_cash"

failures = []


def check(held, said):
    print(f"{'ok' if held else 'FAILED'}: {said}", flush=True)
    if not held:
        failures.append(said)


def context(browser, width=1440, height=900, script=True):
    ctx = browser.new_context(
        viewport={"width": width, "height": height}, java_script_enabled=script
    )
    ctx.add_cookies([{"name": "meridian_session", "value": SESSION, "url": BASE}])
    return ctx


FIT = """async () => {
  const kit = document.querySelector('link[rel=stylesheet][href$="/meridian.css"]');
  const fit = await import(kit.href.replace(/meridian\\.css$/, "lib/fit.js"));
  return fit.fitProblems(fit.measureFit());
}"""


def settled(page):
    page.wait_for_load_state("load")
    page.wait_for_timeout(400)


def fits(browser, name, url, prefix):
    """The page at both sizes, and each of its groups' tabs, by the kit's check."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(url)
        settled(page)
        groups = page.eval_on_selector_all(
            "nav.setting-groups a[href^='#']", "links => links.map(a => a.getAttribute('href'))"
        )
        for group in [None] + groups[1:]:
            if group:
                page.evaluate("h => { location.hash = h; }", group)
                page.wait_for_timeout(200)
            label = f"{name}{group or ''} at {size}"
            problems = page.evaluate(FIT)
            check(not problems, f"{label} fits one screen{': ' + '; '.join(problems) if problems else ''}")
            shot = f"{prefix}-{name}{('-' + group[1:]) if group else ''}-{size}.png"
            page.screenshot(path=os.path.join(OUT, shot))
        # A commercial key brings two more required fields into view.
        commercial = page.locator("input[name='value.key_type'][value=commercial]")
        if commercial.count():
            page.evaluate("h => { location.hash = h; }", groups[0] if groups else "")
            commercial.check()
            page.wait_for_timeout(200)
            problems = page.evaluate(FIT)
            check(
                not problems,
                f"{name} with a commercial key at {size} fits one screen"
                f"{': ' + '; '.join(problems) if problems else ''}",
            )
            page.screenshot(path=os.path.join(OUT, f"{prefix}-{name}-commercial-{size}.png"))
        check(not errors, f"{name} at {size} ran without a script error: {errors}")
        ctx.close()


def grid_rows(page):
    return page.evaluate("() => document.querySelector('om-entry-grid').size")


def held_badge(page):
    return page.inner_text("[data-rows]")


def the_grid_adds_rows_past_four_and_posts_them(browser):
    ctx = context(browser)
    page = ctx.new_page()
    page.goto(AREA + PLAN)
    settled(page)
    upgraded = page.evaluate(
        "() => !!customElements.get('om-entry-grid') && !!document.querySelector('om-entry-grid table.om-entry')"
    )
    check(upgraded, "the Plan-code links tab's grid is the kit's: the component loaded and drew its table")
    check(held_badge(page).startswith("1 of at most 200"), f"it holds one link: {held_badge(page)}")
    add = page.locator("button.om-entry-add")
    check(add.count() == 1, "it offers Add a row")
    for n in range(2, 7):
        add.click()
        row = page.locator("table.om-entry > tbody > tr[data-row]:not(.om-entry-paged)").last
        row.locator("select").nth(0).select_option("FID-IRA-2" if n % 2 else "SCHW-BRK-3")
        row.locator("input").first.fill(f"PLAN{n}")
        row.locator("select").nth(1).select_option(f"INS-{n:04d}")
    check(grid_rows(page) == 6, f"six rows typed, past the four the page once offered: {grid_rows(page)}")
    page.click("form[data-table-setting] .form-foot button[type=submit]")
    settled(page)
    check("tab=" + PLAN in page.url and "saved=1" in page.url, f"saved, back on its tab: {page.url}")
    check(held_badge(page).startswith("6 of at most 200"), f"the six are held: {held_badge(page)}")
    check(grid_rows(page) == 6, "and the grid shows them")
    check(
        "Ada Admin" in page.inner_text("[data-last-changed]"),
        f"the latest stamped with who: {page.inner_text('[data-last-changed]')}",
    )
    ctx.close()


def two_hundred_are_accepted_and_two_hundred_and_one_refused(browser):
    ctx = context(browser)
    page = ctx.new_page()
    page.goto(AREA + CASH)
    settled(page)
    lines = "\n".join(f"VG-ROTH-4\tFDIC{n:05d}\tUSD" for n in range(1, 202))
    paste = """text => {
      const cell = document.querySelector('table.om-entry > tbody > tr[data-row] select, table.om-entry > tbody > tr[data-row] input');
      const data = new DataTransfer();
      data.setData('text/plain', text);
      cell.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
    }"""
    page.evaluate(paste, lines)
    page.wait_for_timeout(300)
    check(grid_rows(page) == 200, f"201 rows pasted fill the grid to its most, 200: {grid_rows(page)}")
    check(page.locator("button.om-entry-add").is_disabled(), "Add a row offers no 201st: it is disabled")
    page.click("form[data-table-setting] .form-foot button[type=submit]")
    settled(page)
    check(held_badge(page).startswith("200 of at most 200"), f"200 rows are accepted and held: {held_badge(page)}")

    # 201 posted anyway, as the form would post them, with the page's token.
    form = page.locator("form[data-table-setting]")
    action = form.get_attribute("action")
    token = form.locator("input[type=hidden]").first
    fields = {token.get_attribute("name"): token.get_attribute("value"), "table.counted_as_cash": "1"}
    for n in range(201):
        fields[f"table.counted_as_cash[{n}].account"] = "VG-ROTH-4"
        fields[f"table.counted_as_cash[{n}].symbol"] = f"FDIC{n:05d}"
        fields[f"table.counted_as_cash[{n}].currency"] = "USD"
    answer = page.request.post(
        BASE + action,
        data=urlencode(fields),
        headers={"content-type": "application/x-www-form-urlencoded"},
        max_redirects=0,
    )
    body = answer.text()
    check(
        answer.status == 400 and "at most 200 rows" in body,
        f"201 rows are refused, naming the most: {answer.status} {re.sub(r'<[^>]+>', ' ', body)[:160]!r}",
    )
    page.reload()
    settled(page)
    check(held_badge(page).startswith("200 of at most 200"), f"and the 200 stand: {held_badge(page)}")
    ctx.close()


def without_script_the_plain_table_posts(browser):
    ctx = context(browser, script=False)
    page = ctx.new_page()
    page.goto(PORTAL + PLAN)
    rows = page.locator("om-entry-grid table tbody tr")
    held = page.inner_text("[data-rows]")
    count = int(held.split()[0])
    check(rows.count() == count + 3, f"without script, the rows held and three blank: {rows.count()} for {held}")
    blank = rows.nth(count)
    blank.locator("select").nth(0).select_option("FID-401K-1")
    blank.locator("input").first.fill("NOSCRIPT")
    blank.locator("select").nth(1).select_option("INS-0042")
    page.click("form[data-table-setting] .form-foot button[type=submit]")
    page.wait_for_load_state("load")
    check(
        "tab=" + PLAN in page.url and page.inner_text("[data-rows]").startswith(f"{count + 1} of"),
        f"and it posts, back on the admin portal's tab: {page.inner_text('[data-rows]')}",
    )
    ctx.close()


ADMIN_PAGE = f"{BASE}/admin"
TWO_ROLES = "ops-1"


def inside_the_viewport(page):
    """The open dialog's box within the viewport, as one screen."""
    return page.evaluate("""() => {
      const d = document.querySelector('dialog[open]');
      if (!d) return 'no dialog open';
      const r = d.getBoundingClientRect();
      return (r.top >= 0 && r.left >= 0 && r.bottom <= innerHeight + 1 && r.right <= innerWidth + 1)
        ? '' : `the dialog reaches ${Math.round(r.right)}x${Math.round(r.bottom)}`;
    }""")


def the_access_editor_has_a_row_per_role(browser, prefix):
    """Access per role (contract v15): the access group dialog's rows, one a
    plugin role, one line each, paged by the kit; an entry on a role its
    plugin no longer holds flagged; the per-plugin Access tab's rows by role;
    each page and the open editor fitting one screen at both sizes."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(ADMIN_PAGE + "#access-groups")
        settled(page)
        problems = page.evaluate(FIT)
        check(not problems, f"the Access groups tab at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-access-groups-{size}.png"))
        listed = page.inner_text("#access-groups")
        check("ops-1 operations write" in listed and "ops-1 custody read" in listed,
              f"at {size} each group lists its entries by role")
        check("holds nothing" in listed, f"at {size} the entry on a role ops-1 no longer holds is flagged")

        page.click("tr[data-id='AX-RECON'] button[data-dialog-open]")
        page.wait_for_timeout(500)
        editor = page.locator("dialog#access-group om-pager")
        check(editor.count() == 1, f"at {size} the Access editor is paged by the kit's om-pager")
        rows = page.locator("dialog#access-group table.access-roles > tbody > tr")
        shown = page.evaluate("""() => [...document.querySelectorAll('dialog#access-group table.access-roles > tbody > tr')]
          .filter(r => r.offsetParent !== null).length""")
        check(rows.count() >= 24 and 0 < shown <= 6,
              f"at {size} a row per plugin role ({rows.count()}), six a page ({shown} shown)")
        check(page.input_value("select[name='level.ops-1:operations']") == "write"
              and page.input_value("select[name='level.ops-1:custody']") == "read",
              f"at {size} the Edit fills each role's level")
        pager = page.evaluate("() => document.querySelector('dialog#access-group om-pager').shown")
        check(pager and pager.get("total", 0) == rows.count(),
              f"at {size} the pager counts every row: {pager}")
        wraps = page.evaluate(FIT)
        check(not wraps, f"the open Access editor at {size} fits one screen"
              f"{': ' + '; '.join(wraps) if wraps else ''}")
        boxed = inside_the_viewport(page)
        check(not boxed, f"at {size} the Access editor stays within the viewport{': ' + boxed if boxed else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-access-editor-{size}.png"))
        if size == "phone" and PASS != "fit":
            page.select_option("select[name='level.ops-1:custody']", "admin-read")
            page.click("dialog#access-group button[type=submit]")
            settled(page)
            listed = page.text_content("#access-groups")
            check("ops-1 custody admin" in listed and "ops-1 custody read" in listed,
                  f"a role's level posted from the editor is held: {listed[:200]!r}")

        page.goto(f"{BASE}/admin/plugins/{TWO_ROLES}?tab=access")
        settled(page)
        problems = page.evaluate(FIT)
        check(not problems, f"ops-1's Access tab at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        tab = page.text_content("main")
        check("Role" in tab and "operations" in tab and "custody" in tab,
              f"at {size} ops-1's Access tab lists each grant's role")
        check("holds nothing: ops-1 holds custody, operations" in tab,
              f"at {size} the grant on a role ops-1 no longer holds is flagged naming its roles")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-access-tab-{size}.png"))
        check(not errors, f"the access pages at {size} ran without a script error: {errors}")
        ctx.close()


with sync_playwright() as playwright:
    browser = playwright.chromium.launch()
    prefix = "development" if PASS == "fit" else "settings"
    if PASS != "fit":
        the_grid_adds_rows_past_four_and_posts_them(browser)
        two_hundred_are_accepted_and_two_hundred_and_one_refused(browser)
        without_script_the_plain_table_posts(browser)
    the_access_editor_has_a_row_per_role(browser, prefix)
    for name, url in [
        ("area-settings", AREA + "settings"),
        ("area-plan-code-links", AREA + PLAN),
        ("area-cash-links", AREA + CASH),
        ("portal-settings", PORTAL + "settings"),
        ("portal-plan-code-links", PORTAL + PLAN),
        ("portal-cash-links", PORTAL + CASH),
    ]:
        fits(browser, name, url, prefix)
    browser.close()

if failures:
    print(f"{len(failures)} check(s) failed", flush=True)
    sys.exit(1)
print("settings pages: every check held", flush=True)
