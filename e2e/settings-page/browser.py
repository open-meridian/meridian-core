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
  and 390x844 by the kit's own overflow check (lib/fit.js, fitProblems), and
  nothing in the open Access editor reaches past its dialog;
- the Open Meridian icon: each page's head links the PNG, then the SVG, then
  the touch icon, each answered with its type, the SVG drawn light and dark,
  and /favicon.ico answered unprompted;
- an edge plugin's raw records (contract v16): its Summary's panel, one line
  a kind of what storage and the archive hold, its moves paged by the kit's
  om-pager, which upgrades; Allow archive's dialog within the viewport;
  allowing with a bound and withdrawing, each back on the Summary saying so;
  and the deployment's Holds tab and its dialog, a hold set from it held --
  each page and open dialog fitting one screen at both sizes;
- the Data sources page (contract v18): its Datasets, Entitlements and
  Priority tabs, one line a row on the kit's pager, which upgrades, with no
  cell cut off; its licence, entitlement and priority dialogs within the
  viewport; a licence set and a priority changed from them held, and a
  priority sent against what was read before refused as changed -- each tab
  and open dialog fitting one screen at both sizes.

- the Accounts tab (2026-10-10, the Books tab folded in): one line an
  account, its base currency and lot relief beside its custodian and type,
  searched; its Edit of two parts, "Account" and "In the book", each its own
  form, the second only for an account that exists and saving to the book;
  the old #books address opening it -- the tab and the dialog in one screen
  at both sizes;
- a secret's field (2026-10-10): who set it and when, or "Not set", in the
  field itself; no badge and no box; a Remove beside an optional secret
  alone, which asks first, declined sending nothing and confirmed removing it.
- the Service accounts tab (contract v21): one line an account behind the
  kit's pager, its dialog within the viewport, one created from it prepare
  only by default; and the Access tab's Runs as, one line a role, nobody
  until chosen, its dialog within the viewport -- each at both sizes.

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
        # An instrument cell searches core as it is typed (kit 0.12.0), and
        # answers an exact ID: the match is chosen from the list.
        row.locator("input[data-key=instrument]").fill(f"INS-{n:04d}")
        page.locator("ul.om-picker-list:not([hidden]) [data-index='0']").click()
    check(grid_rows(page) == 6, f"six rows typed, past the four the page once offered: {grid_rows(page)}")
    page.click("form[data-table-setting] .form-foot button[data-save]")
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
    page.click("form[data-table-setting] .form-foot button[data-save]")
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
    blank.locator("input").nth(1).fill("INS-0042")
    page.click("form[data-table-setting] .form-foot button[data-save]")
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


# The kit's overflow check, turned on the open dialog: the kit's own
# fitProblems reads the document, and a modal dialog sits in the top layer,
# fixed to the viewport, which it does not count. So the same rule, held to
# the dialog's box: nothing in it, that nothing scrolls or clips, reaches past
# its edges, and it scrolls no wider than it is. Its one-line rows are the
# kit's check's already, which counts every table.one-line row drawn.
INSIDE = """() => {
  const d = document.querySelector('dialog[open]');
  if (!d) return ['no dialog open'];
  const box = d.getBoundingClientRect();
  const name = (el) => el.localName + (el.id ? '#' + el.id : '')
    + [...el.classList].slice(0, 2).map((c) => '.' + c).join('');
  const out = [];
  if (d.scrollWidth > d.clientWidth + 1)
    out.push(`the dialog is ${d.scrollWidth - d.clientWidth}px too wide inside`);
  (function walk(el, clipped) {
    for (const child of [...el.children, ...(el.shadowRoot ? el.shadowRoot.children : [])]) {
      const st = getComputedStyle(child);
      if (st.display === 'none') continue;
      const r = child.getBoundingClientRect();
      if (!clipped && (r.width || r.height)
          && (r.right > box.right + 0.5 || r.left < box.left - 0.5))
        out.push(`${name(child)} reaches ${Math.round(r.left)} to ${Math.round(r.right)}px, past the dialog's ${Math.round(box.left)} to ${Math.round(box.right)}px`);
      walk(child, clipped || st.overflowX !== 'visible' || st.overflowY !== 'visible');
    }
  })(d, false);
  return out;
}"""


def on_its_page(page, row):
    """Turn the kit's pager holding `row` (kit 0.11.0 pages Settings' lists)
    until the row is shown."""
    page.evaluate("""row => {
      const found = document.querySelector(row);
      const pager = found && found.closest('om-pager');
      for (let i = 0; pager && i < 50 && found.offsetParent === null; i++) pager.turn(1);
    }""", row)
    page.wait_for_timeout(100)


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
        # Every group, on whichever page the kit's pager shows it (kit
        # 0.11.0 pages Settings' lists): its whole text, spaces collapsed.
        listed = " ".join(page.eval_on_selector("#access-groups", "e => e.textContent").split())
        check("ops-1 operations write" in listed and "ops-1 custody read" in listed,
              f"at {size} each group lists its entries by role")
        check("holds nothing" in listed, f"at {size} the entry on a role ops-1 no longer holds is flagged")

        on_its_page(page, "tr[data-id='AX-RECON']")
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
        spilled = page.evaluate(INSIDE)
        check(not spilled, f"at {size} nothing in the Access editor reaches past the dialog"
              f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
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


SUMMARY = AREA + "summary"

# The cells of a one-line table whose text the ellipsis cuts: each drawn
# cell's content wider than its box. A phone's narrower cells leave their
# dates and times to the cell's title, so nothing drawn is cut either, but a
# kind's label, which its title holds whole.
CUT = """(table) => [...document.querySelectorAll(table + ' :is(th, td)')]
  .filter((c) => innerWidth > 640 || !c.closest('table.kinds') || c.cellIndex > 0)
  .filter((c) => c.offsetParent !== null && c.scrollWidth > c.clientWidth + 1)
  .map((c) => c.textContent.trim())"""


def the_raw_records_and_the_holds(browser, prefix):
    """Contract v16: the Summary's raw records panel, Allow archive on it,
    and the holds on the deployment's Settings, at both sizes."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(SUMMARY)
        settled(page)
        parts = page.eval_on_selector_all("nav.summary-parts a", "links => links.map(a => a.getAttribute('href'))")
        check(parts[:4] == ["#part-status", "#part-setting-up", "#part-records", "#part-moves"],
              f"at {size} the Summary's parts: its status, Setting up (contract v20), then the raw "
              f"records and their moves: {parts}")
        for part in parts:
            page.evaluate("h => { location.hash = h; }", part)
            page.wait_for_timeout(300)
            problems = page.evaluate(FIT)
            check(not problems, f"the Summary's {part} at {size} fits one screen"
                  f"{': ' + '; '.join(problems) if problems else ''}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-summary-{part[6:]}-{size}.png"))
        page.evaluate("h => { location.hash = h; }", "#part-records")
        page.wait_for_timeout(300)
        panel = page.locator("#records")
        check(panel.count() == 1 and panel.is_visible(), f"at {size} the Summary draws the raw records panel")
        kinds = page.eval_on_selector_all("#records table.kinds tbody tr", "rows => rows.map(r => r.dataset.kind)")
        check(kinds == ["activity", "responses"], f"at {size} one line a kind: {kinds}")
        stored = page.get_attribute("#records tr[data-kind=activity] td[data-stored]", "title")
        archived = page.get_attribute("#records tr[data-kind=activity] td[data-archived]", "title")
        check(stored.startswith("48,210, 2019-04-01 to "), f"at {size} what storage holds: {stored}")
        check(archived.startswith("12,570, 2014-01-01 to "), f"at {size} what the archive holds: {archived}")
        used = page.inner_text("#records tr[data-kind=activity] td[data-used]")
        none = page.inner_text("#records tr[data-kind=responses] td[data-used]")
        check(used == "3.2 GiB" and none == "none", f"at {size} the bytes each kind uses of the archive: {used}, {none}")
        foot = page.locator("#records tfoot tr[data-used-in-all]")
        check(foot.count() == 1 and foot.is_visible() and "3.2 GiB" in foot.inner_text(),
              f"at {size} every kind's bytes together, in the table's foot")
        cut = page.evaluate(CUT, "#records table.kinds")
        check(not cut, f"at {size} no cell of the raw records is cut off: {cut}")
        state = page.inner_text("#records .archive-state")
        check("Archive withdrawn" in state and "3.2 GiB" in state and "No archive allowed" not in state,
              f"at {size} records in an archive no longer allowed: withdrawn, what it holds kept: {state}")
        held = page.inner_text("#records [data-hold]")
        check("2,190 days" in held and "responses_window_days" in held,
              f"at {size} the hold it is under, and the window it overrides: {held}")
        page.evaluate("h => { location.hash = h; }", "#part-moves")
        page.wait_for_timeout(500)
        upgraded = page.evaluate("() => !!customElements.get('om-pager') && !!document.querySelector('#moves om-pager nav')")
        shown = page.evaluate("""() => [...document.querySelectorAll('#moves table.moves tbody tr')]
          .filter(r => r.offsetParent !== null).length""")
        total = page.locator("#moves table.moves tbody tr").count()
        check(upgraded and 3 <= shown < total,
              f"at {size} the moves are paged by the kit's om-pager, a screen's worth a page: {shown} of {total} shown")
        pager = page.evaluate("() => document.querySelector('#moves om-pager').shown")
        check(bool(pager) and pager.get("total", 0) == total, f"at {size} the pager counts every move: {pager}")
        check(page.locator("#moves a.older").count() == 1, f"at {size} the conductor's older moves a link away")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-summary-moves-paged-{size}.png"))
        page.evaluate("h => { location.hash = h; }", "#part-records")
        page.wait_for_timeout(300)
        if page.locator("#records [data-archive=withdrawn]").count():
            page.click("#records [data-allow-archive]")
            page.wait_for_timeout(300)
            boxed = inside_the_viewport(page)
            check(not boxed, f"at {size} Allow archive's dialog stays within the viewport{': ' + boxed if boxed else ''}")
            spilled = page.evaluate(INSIDE)
            check(not spilled, f"at {size} nothing in Allow archive's dialog reaches past it"
                  f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-allow-archive-{size}.png"))
            if size == "desktop" and PASS != "fit":
                page.fill("dialog#allow-archive input[name=most_gib]", "50")
                page.click("dialog#allow-archive button.primary[type=submit]")
                settled(page)
                said = page.inner_text("#records .archive-state")
                check("saved=1" in page.url and page.locator("#records").is_visible()
                      and "Archive allowed: 3.2 GiB of at most 50 GiB used." in said,
                      f"an archive allowed with a bound, back on the Summary, its use against it: {said} ({page.url})")
                foot = page.inner_text("#records tfoot tr[data-used-in-all]")
                check("3.2 GiB of 50 GiB" in foot, f"every kind together against the bound: {foot}")
                problems = page.evaluate(FIT)
                check(not problems, f"the Summary with its archive allowed fits one screen"
                      f"{': ' + '; '.join(problems) if problems else ''}")
                page.screenshot(path=os.path.join(OUT, f"{prefix}-summary-archive-allowed-{size}.png"))
                page.click("#records [data-withdraw-archive]")
                settled(page)
                said = page.inner_text("#records .archive-state")
                check("Archive withdrawn" in said, f"withdrawn, it says so again: {said}")

        page.goto(ADMIN_PAGE + "#holds")
        settled(page)
        rows = page.eval_on_selector_all("#holds table.holds tbody tr", "rows => rows.map(r => r.dataset.id)")
        check(set(rows) >= {"", "custody"}, f"at {size} the Holds tab lists each hold, one line each: {rows}")
        cut = page.evaluate(CUT, "#holds table.holds")
        check(not cut, f"at {size} no cell of the holds is cut off, when each was set among them: {cut}")
        problems = page.evaluate(FIT)
        check(not problems, f"the Holds tab at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-holds-{size}.png"))
        page.click("#holds button[data-dialog-open=hold]")
        page.wait_for_timeout(300)
        boxed = inside_the_viewport(page)
        check(not boxed, f"at {size} the hold's dialog stays within the viewport{': ' + boxed if boxed else ''}")
        spilled = page.evaluate(INSIDE)
        check(not spilled, f"at {size} nothing in the hold's dialog reaches past it"
              f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-hold-dialog-{size}.png"))
        if size == "desktop" and PASS != "fit":
            page.select_option("dialog#hold select[name=role]", "settlement")
            page.fill("dialog#hold input[name=days]", "3650")
            page.click("dialog#hold button.primary[type=submit]")
            settled(page)
            listed = page.text_content("#holds")
            check("settlement" in listed and "3,650 days" in listed, f"a hold set from the dialog is held: {listed[:200]!r}")
        check(not errors, f"the raw records and holds at {size} ran without a script error: {errors}")
        ctx.close()


def the_accounts_tab_holds_the_book(browser, prefix):
    """The product owner, 2026-10-10: one book per deployment, so the Books
    tab is folded into Accounts. One line an account -- its name, custodian
    and type, and in the book its base currency and lot relief -- searched;
    one Edit of two parts, "Account" and "In the book", each its own form,
    the second only for an account that exists; the old #books address
    opening Accounts; at both sizes, the tab and the open dialog in one
    screen."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(ADMIN_PAGE + "#books")
        settled(page)
        check(page.locator("#accounts").is_visible() and not page.locator("#books").count(),
              f"at {size} the old Books address opens Accounts, and no Books tab is left")
        heads = page.eval_on_selector_all("#accounts-table thead th", "hs => hs.map(h => h.textContent)")
        check(heads[:5] == ["Name", "Custodian", "Type", "Base currency", "Lot relief"],
              f"at {size} each account's row says its name, custodian, type, base currency and lot relief: {heads}")
        rows = page.eval_on_selector_all("#accounts-table tbody tr", "rows => rows.map(r => r.dataset.name)")
        check(len(rows) == 6, f"at {size} every account is listed: {rows}")
        tall = page.evaluate("""() => [...document.querySelectorAll('#accounts-table tbody tr')]
          .filter((r) => r.offsetParent !== null && r.getBoundingClientRect().height > 64).map((r) => r.dataset.name)""")
        check(not tall, f"at {size} each account is one line: {tall}")
        roth = page.inner_text("#accounts-table tr[data-name='Brokerage']")
        check("USD" in roth and "Highest cost" in roth, f"at {size} the book's attributes on the row: {roth!r}")
        problems = page.evaluate(FIT)
        check(not problems, f"the Accounts tab at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-accounts-{size}.png"))
        page.fill("input[data-filter='accounts-table']", "vanguard")
        page.wait_for_timeout(200)
        shown = page.eval_on_selector_all("#accounts-table tbody tr",
                                          "rows => rows.filter(r => r.offsetParent !== null).map(r => r.dataset.name)")
        check(shown == ["Roth IRA"], f"at {size} the search finds an account by its custodian: {shown}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-accounts-search-{size}.png"))
        page.fill("input[data-filter='accounts-table']", "")
        page.wait_for_timeout(200)
        page.click("#accounts-table tr[data-name='Credit Suisse 401(k)'] button[data-dialog-open=account]")
        page.wait_for_timeout(300)
        parts = page.eval_on_selector_all("dialog#account form:not([hidden]) h3",
                                          "hs => hs.map(h => h.firstChild.textContent.trim())")
        check(parts == ["Account", "In the book"], f"at {size} the Edit holds both parts: {parts}")
        filled = page.evaluate("""() => { const d = document.querySelector('dialog#account');
          return [d.querySelector('[name=name]').value, d.querySelector('[name=owner]').value,
                  d.querySelector('[name=base_currency_code]').value, d.querySelector('output[name=since]').value]; }""")
        # Its currency USD, or EUR once the desktop's run saved it so.
        check(filled[:2] == ["Credit Suisse 401(k)", "Vincent"] and filled[2] in ("USD", "EUR")
              and filled[3] == "2026-09-08",
              f"at {size} the Edit is filled with the account and its book: {filled}")
        boxed = inside_the_viewport(page)
        check(not boxed, f"at {size} the account's dialog stays within the viewport{': ' + boxed if boxed else ''}")
        spilled = page.evaluate(INSIDE)
        check(not spilled, f"at {size} nothing in the account's dialog reaches past it"
              f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-account-edit-{size}.png"))
        if size == "desktop" and PASS != "fit":
            page.fill("dialog#account [name=base_currency_code]", "EUR")
            page.fill("dialog#account form[data-edit-only] [name=note]", "The plan reports in euros")
            page.click("dialog#account form[data-edit-only] button[type=submit]")
            settled(page)
            row = page.inner_text("#accounts-table tr[data-name='Credit Suisse 401(k)']")
            check("EUR" in row and page.locator("#accounts").is_visible(),
                  f"saved in the book from the Edit, back on Accounts: {row!r}")
        else:
            page.click("dialog#account [data-dialog-close]")
            page.wait_for_timeout(200)
        page.click("#accounts button[data-dialog-open=account][aria-label]")
        page.wait_for_timeout(300)
        hidden = page.evaluate("() => document.querySelector('dialog#account form[data-edit-only]').hidden")
        check(hidden, f"at {size} a new account's dialog has no In the book")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-account-new-{size}.png"))
        check(not errors, f"the Accounts tab at {size} ran without a script error: {errors}")
        ctx.close()


def the_service_accounts_tab_and_runs_as(browser, prefix):
    """Contract v21 (W6.32, W6.33): the deployment's Service accounts tab,
    one line an account behind the kit's pager -- its name, owner, setting,
    state, the roles it runs for, last used and last refusal -- its dialog
    within the viewport, one created from it, prepare only by default; and a
    plugin's Access tab's Runs as, one line a role, its dialog within the
    viewport -- each page and open dialog fitting one screen at both sizes."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(ADMIN_PAGE + "#service-accounts")
        settled(page)
        check(page.locator("#service-accounts").is_visible(), f"at {size} the Service accounts tab opens")
        if size == "desktop" and PASS != "fit":
            page.click("#service-accounts button[data-dialog-open=service-account][aria-label]")
            page.wait_for_timeout(300)
            chosen = page.evaluate("() => document.querySelector('dialog#service-account [name=setting]').value")
            check(chosen == "prepare_only", f"a new service account is prepare only by default: {chosen}")
            page.fill("dialog#service-account [name=name]", "The nightly sweep")
            page.fill("dialog#service-account [name=purpose]", "Closes breaks whose difference has gone")
            page.select_option("dialog#service-account [name=owner_subject]", index=1)
            page.fill("dialog#service-account [name=note]", "For the reconciliation sweep")
            page.click("dialog#service-account button[type=submit]")
            settled(page)
            row = page.inner_text("#service-accounts-table tr[data-name='The nightly sweep']")
            check("Prepare only" in row and "Active" in row, f"created from the dialog, one line: {row!r}")
        heads = page.eval_on_selector_all("#service-accounts-table thead th", "hs => hs.map(h => h.textContent)")
        check(heads[:7] == ["Name", "Owner", "Setting", "State", "Runs for", "Last used", "Last refusal"],
              f"at {size} each account's row says its name, owner, setting, state, the roles it runs for, "
              f"last used and last refusal: {heads}")
        tall = page.evaluate("""() => [...document.querySelectorAll('#service-accounts-table tbody tr')]
          .filter((r) => r.offsetParent !== null && r.getBoundingClientRect().height > 64).map((r) => r.dataset.name)""")
        check(not tall, f"at {size} each service account is one line: {tall}")
        problems = page.evaluate(FIT)
        check(not problems, f"the Service accounts tab at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-service-accounts-{size}.png"))
        page.click("#service-accounts button[data-dialog-open=service-account][aria-label]")
        page.wait_for_timeout(300)
        boxed = inside_the_viewport(page)
        check(not boxed, f"at {size} the service account's dialog stays within the viewport{': ' + boxed if boxed else ''}")
        spilled = page.evaluate(INSIDE)
        check(not spilled, f"at {size} nothing in the service account's dialog reaches past it"
              f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-service-account-new-{size}.png"))
        page.click("dialog#service-account [data-dialog-close]")
        page.wait_for_timeout(200)

        page.goto(f"{BASE}/admin/plugins/{TWO_ROLES}?tab=access#part-runs-as")
        settled(page)
        roles = page.eval_on_selector_all("#runs-as-table tbody tr", "rows => rows.map(r => r.dataset.name)")
        check(len(roles) >= 1, f"at {size} Runs as has one line a role: {roles}")
        nobody = page.inner_text("#runs-as-table tbody tr td[data-runs-as='']") if roles else ""
        check(nobody.startswith("nobody"), f"at {size} a role runs as nobody until its admin chooses: {nobody!r}")
        problems = page.evaluate(FIT)
        check(not problems, f"the Access tab's Runs as at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-runs-as-{size}.png"))
        edit = page.locator("#runs-as-table button[data-dialog-open]").first
        if edit.count():
            edit.click()
            page.wait_for_timeout(300)
            boxed = inside_the_viewport(page)
            check(not boxed, f"at {size} the Runs as dialog stays within the viewport{': ' + boxed if boxed else ''}")
            spilled = page.evaluate(INSIDE)
            check(not spilled, f"at {size} nothing in the Runs as dialog reaches past it"
                  f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-runs-as-edit-{size}.png"))
        check(not errors, f"the service accounts' pages at {size} ran without a script error: {errors}")
        ctx.close()


def the_connectivity_part_and_the_calendars_tab(browser, prefix):
    """Contract v22 (W11.21, W11.22, W3.21): a connectivity plugin's Access
    tab says whether it is available, the ems it is linked to and who may
    route, one line, its dialogs within the viewport, withdrawn and made
    available again from its dialog; an ems's says its connectivity plugins,
    its admin linking one there from a dialog within the viewport that
    returns to it (W11.21), and its venue account map, one line a mapping behind the kit's pager,
    read-only; the Instruments page's Calendars tab lists the calendars held
    one line each and shows a calendar's year -- each fitting one screen at
    both sizes. From contract v23 (W3.21) each line shows its subject -- the
    currency's ISO 4217 code, the venues by their IDs, the settlement
    system's name and country -- and the tab is filtered by kind and searched
    by text, its one form a GET that sets nothing."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(f"{BASE}/admin/plugins/fix-1?tab=access#part-connectivity")
        settled(page)
        shown = page.locator("table.connectivity").is_visible()
        check(shown, f"at {size} a connectivity plugin's Access tab shows its ems")
        if shown:
            row = page.inner_text("table.connectivity tbody tr")
            check("ems-1" in row, f"at {size} it says the ems it is linked to: {row!r}")
        problems = page.evaluate(FIT)
        check(not problems, f"a connectivity plugin's ems at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-connectivity-{size}.png"))
        for dialog in ("connectivity-link", "connectivity-available"):
            opener = page.locator(f"button[data-dialog-open={dialog}]")
            if not opener.count():
                check(False, f"at {size} the {dialog} dialog is offered to the deployment's admin")
                continue
            opener.click()
            page.wait_for_timeout(300)
            boxed = inside_the_viewport(page)
            check(not boxed, f"at {size} the {dialog} dialog stays within the viewport{': ' + boxed if boxed else ''}")
            spilled = page.evaluate(INSIDE)
            check(not spilled, f"at {size} nothing in the {dialog} dialog reaches past it"
                  f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-{dialog}-{size}.png"))
            if dialog == "connectivity-available" and size == "desktop" and PASS != "fit":
                page.fill("dialog#connectivity-available [name=note]", "The venue is paused for the day")
                page.click("dialog#connectivity-available button[type=submit]")
                settled(page)
                page.goto(f"{BASE}/admin/plugins/fix-1?tab=access#part-connectivity")
                settled(page)
                withdrawn = page.get_attribute("table.connectivity td[data-available]", "data-available")
                check(withdrawn == "false", f"withdrawn from its dialog: {withdrawn!r}")
                page.click("button[data-dialog-open=connectivity-available]")
                page.wait_for_timeout(300)
                page.click("dialog#connectivity-available button[type=submit]")
                settled(page)
                page.goto(f"{BASE}/admin/plugins/fix-1?tab=access#part-connectivity")
                settled(page)
                back = page.get_attribute("table.connectivity td[data-available]", "data-available")
                check(back == "true", f"made available again from its dialog: {back!r}")
            else:
                page.click(f"dialog#{dialog} [data-dialog-close]")
                page.wait_for_timeout(200)

        page.goto(f"{BASE}/admin/plugins/ems-1?tab=access#part-connectivity")
        settled(page)
        mapped = page.get_attribute("[data-mapped]", "data-mapped")
        check((mapped or "0").isdigit() and int(mapped or "0") >= 6,
              f"at {size} an ems's venue account map lists its mappings: {mapped!r}")
        check(page.locator("#connectivity form:not([action$='/connectivity/link'])").count() == 0,
              f"at {size} an ems's map is read-only here, set on its own page")
        # W11.21: the ems's admin links its connectivity plugins here, on the
        # ems's page, and the change returns here.
        opener = page.locator("#connectivity tr[data-connectivity=fix-1] button[data-dialog-open]")
        if not opener.count():
            check(False, f"at {size} the ems's admin is offered fix-1's link on the ems's page")
        else:
            dialog = opener.get_attribute("data-dialog-open")
            opener.click()
            page.wait_for_timeout(300)
            boxed = inside_the_viewport(page)
            check(not boxed, f"at {size} the ems's link dialog stays within the viewport{': ' + boxed if boxed else ''}")
            spilled = page.evaluate(INSIDE)
            check(not spilled, f"at {size} nothing in the ems's link dialog reaches past it"
                  f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
            chosen = page.locator(f"dialog#{dialog} input[name=user]:checked").count()
            check(chosen >= 1, f"at {size} its dialog has who may route through fix-1 chosen: {chosen}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-ems-link-{size}.png"))
            if size == "desktop" and PASS != "fit":
                page.fill(f"dialog#{dialog} [name=note]", "Kept as it is, saved from the ems")
                page.click(f"dialog#{dialog} button[type=submit]")
                settled(page)
                check("/admin/plugins/ems-1" in page.url,
                      f"saved from the ems's page, it returns there: {page.url}")
                page.goto(f"{BASE}/admin/plugins/ems-1?tab=access#part-connectivity")
                settled(page)
                linked = page.get_attribute("[data-linked]", "data-linked")
                check(linked == "1", f"and fix-1 is still linked to ems-1: {linked!r}")
            else:
                page.click(f"dialog#{dialog} [data-dialog-close]")
                page.wait_for_timeout(200)
        tall = page.evaluate("""() => [...document.querySelectorAll('[data-mapped] tbody tr')]
          .filter((r) => r.offsetParent !== null && r.getBoundingClientRect().height > 64).length""")
        check(not tall, f"at {size} each mapping is one line: {tall} taller")
        problems = page.evaluate(FIT)
        check(not problems, f"an ems's connectivity plugins and map at {size} fit one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-venue-account-map-{size}.png"))

        for name, url, marker in [
            ("calendars", f"{BASE}/admin/instruments#calendars", "[data-calendars]"),
            ("calendars-searched", f"{BASE}/admin/instruments?calendar_kind=CALENDAR_KIND_SECURITIES_SETTLEMENT&calendar_query=dtc#calendars", "[data-calendars]"),
            ("calendar-year", f"{BASE}/admin/instruments?calendar=CAL-01JAE000000000000000USD1&year=2026#calendars", "[data-days]"),
        ]:
            page.goto(url)
            settled(page)
            check(page.locator(marker).count() == 1, f"at {size} the {name} view draws {marker}")
            tall = page.evaluate(f"""() => [...document.querySelectorAll('{marker} tbody tr')]
              .filter((r) => r.offsetParent !== null && r.getBoundingClientRect().height > 64).length""")
            check(not tall, f"at {size} each line of {name} is one line: {tall} taller")
            check(page.locator("#calendars form:not([method=get])").count() == 0,
                  f"at {size} the {name} view sets nothing")
            if name == "calendars":
                usd = page.locator("tr[data-calendar=CAL-01JAE000000000000000USD1] td[data-subject]")
                check(usd.count() == 1 and usd.inner_text().strip() == "USD",
                      f"at {size} the US dollar's subject is its ISO 4217 code: "
                      f"{usd.inner_text() if usd.count() else None!r}")
                venues = page.locator("tr[data-calendar=CAL-01JAE000000000000000XNYS] td[data-subject]")
                check(venues.count() == 1 and "VEN-" in venues.inner_text(),
                      f"at {size} a trading calendar's venues are named by their IDs")
                check(page.locator("#calendars form select[name=calendar_kind]").count() == 1
                      and page.locator("#calendars form input[name=calendar_query]").count() == 1,
                      f"at {size} the tab is filtered by kind and searched by text")
            if name == "calendars-searched":
                shown = page.get_attribute("[data-calendars]", "data-calendars")
                subject = page.locator("td[data-subject]")
                check(shown == "1" and subject.count() == 1 and subject.inner_text().strip() == "DTC, US",
                      f"at {size} the settlement calendars searched for dtc are one, its system "
                      f"and country its subject: {shown!r}")
            problems = page.evaluate(FIT)
            check(not problems, f"the {name} view at {size} fits one screen"
                  f"{': ' + '; '.join(problems) if problems else ''}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-{name}-{size}.png"))
        check(not errors, f"the v22 pages at {size} ran without a script error: {errors}")
        ctx.close()


def a_secret_says_where_it_stands(browser, prefix):
    """The product owner, 2026-10-10: a secret's field says in its
    placeholder who set it and when, or that it is not set; no badge and no
    box; an optional secret has a small Remove that asks first; a required
    one has none."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.goto(PORTAL + "settings")
        settled(page)
        client = page.get_attribute("[data-setting=client_id] input[type=password]", "placeholder")
        check(client.startswith("Set by Ada Admin, "), f"at {size} a set secret says who set it and when: {client!r}")
        check(not page.locator("[data-setting=client_id] [data-remove]").count()
              and not page.locator("form.settings input[type=checkbox][name^='clear.']").count(),
              f"at {size} a required secret has no Remove, and no secret a box")
        consumer = page.get_attribute("[data-setting=consumer_key] input[type=password]", "placeholder")
        check(consumer == "Not set", f"at {size} one not set says so: {consumer!r}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-secret-required-{size}.png"))
        group = page.evaluate("() => document.querySelector('[data-setting=webhook_token]').closest('section').id")
        page.evaluate("h => { location.hash = h; }", "#" + group)
        page.wait_for_timeout(200)
        row = page.locator("[data-setting=webhook_token] .secret-row")
        box = row.bounding_box()
        check(box is not None and box["height"] < 48, f"at {size} the field and its Remove on one line: {box}")
        problems = page.evaluate(FIT)
        check(not problems, f"the secret's group at {size} fits one screen"
              f"{': ' + '; '.join(problems) if problems else ''}")
        page.screenshot(path=os.path.join(OUT, f"{prefix}-secret-optional-{size}.png"))
        if size == "desktop" and PASS != "fit":
            asked = []
            page.once("dialog", lambda d: (asked.append(d.message), d.dismiss()))
            page.click("[data-setting=webhook_token] [data-remove]")
            page.wait_for_timeout(400)
            check(asked and asked[0].startswith("Remove Webhook token?")
                  and page.locator("[data-setting=webhook_token] [data-remove]").count() == 1,
                  f"Remove asks first, and declined, nothing is sent: {asked}")
            page.once("dialog", lambda d: d.accept())
            page.click("[data-setting=webhook_token] [data-remove]")
            settled(page)
            after = page.get_attribute("[data-setting=webhook_token] input[type=password]", "placeholder")
            check(after == "Not set" and not page.locator("[data-setting=webhook_token] [data-remove]").count(),
                  f"confirmed, the secret is removed and says so: {after!r}")
        check(not errors, f"the secret fields at {size} ran without a script error: {errors}")
        ctx.close()


DATA_SOURCES = f"{BASE}/admin/data-sources"

# The cells the ellipsis cuts whose title does not hold their text whole: a
# long dataset name, vendor or order of datasets is cut on its one line, and
# read whole in its title.
CUT_UNTITLED = """(table) => [...document.querySelectorAll(table + ' td')]
  .filter((c) => c.offsetParent !== null && c.scrollWidth > c.clientWidth + 1)
  .filter((c) => !(c.title || '').includes(c.textContent.trim().split(' one person')[0]))
  .map((c) => c.textContent.trim())"""


def the_data_sources_page(browser, prefix):
    """Contract v18: the Data sources page's three tabs and its three
    dialogs, at both sizes, and its forms posted."""
    for size, width, height in SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))
        for tab, table in [("datasets", "table.datasets"), ("entitlements", "table.entitlements"),
                           ("priority", "table.priorities")]:
            page.goto(f"{DATA_SOURCES}#{tab}")
            settled(page)
            upgraded = page.evaluate(
                f"() => !!document.querySelector('#{tab} om-pager') && "
                f"customElements.get('om-pager') !== undefined")
            check(upgraded, f"at {size} the {tab} tab's rows are paged by the kit's om-pager")
            rows = page.eval_on_selector_all(f"#{tab} {table} tbody tr", "rows => rows.length")
            check(rows > 0, f"at {size} the {tab} tab lists its rows: {rows}")
            cut = page.evaluate(CUT_UNTITLED, f"#{tab} {table}")
            check(not cut, f"at {size} no cell of {tab} is cut off but one whose title holds it whole: {cut}")
            problems = page.evaluate(FIT)
            check(not problems, f"the Data sources page's {tab} tab at {size} fits one screen"
                  f"{': ' + '; '.join(problems) if problems else ''}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-data-sources-{tab}-{size}.png"))
        for tab, opener, dialog in [
            ("datasets", "#datasets table.datasets tbody tr:first-child td:first-child button", "licence"),
            ("datasets", "#datasets table.datasets tbody tr:first-child td.actions button[data-dialog-open=entitle]",
             "entitle"),
            ("priority", "#priority button[aria-label='Add a priority']", "priority-dialog"),
        ]:
            page.goto(f"{DATA_SOURCES}#{tab}")
            settled(page)
            page.click(opener)
            page.wait_for_timeout(300)
            boxed = inside_the_viewport(page)
            check(not boxed, f"at {size} the {dialog} dialog stays within the viewport"
                  f"{': ' + boxed if boxed else ''}")
            spilled = page.evaluate(INSIDE)
            check(not spilled, f"at {size} nothing in the {dialog} dialog reaches past it"
                  f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
            page.screenshot(path=os.path.join(OUT, f"{prefix}-data-sources-{dialog}-{size}.png"))
            page.keyboard.press("Escape")
            page.wait_for_timeout(200)
        if size == "desktop" and PASS != "fit":
            page.goto(f"{DATA_SOURCES}#datasets")
            settled(page)
            page.click("#datasets table.datasets tbody tr:first-child td:first-child button")
            page.wait_for_timeout(300)
            # "Kept for" shows once kept is ticked (kit 0.11.0's gate).
            page.check("dialog#licence input[name=kept]")
            check(page.is_visible("dialog#licence input[name=retention_days]"),
                  "the licence's Kept for shows once kept is ticked")
            page.fill("dialog#licence input[name=retention_days]", "3650")
            page.fill("dialog#licence input[name=note]", "Ten years, as the firm now keeps them.")
            page.click("dialog#licence button.primary[type=submit]")
            settled(page)
            listed = page.text_content("#datasets")
            check("kept 3,650 days" in listed, f"a licence set from its dialog is held: {listed[:300]!r}")
            page.goto(f"{DATA_SOURCES}#priority")
            settled(page)
            page.click("#priority table.priorities tbody tr:first-child td.actions button")
            page.wait_for_timeout(300)
            page.fill("dialog#priority-dialog textarea[name=datasets]", "kraken-1:daily\ncoinbase-1:daily")
            page.click("dialog#priority-dialog button.primary[type=submit]")
            settled(page)
            listed = page.text_content("#priority")
            check("kraken-1:daily then coinbase-1:daily" in listed,
                  f"a priority changed from its dialog is held: {listed[:300]!r}")
            # Sent against what was read before the change: refused as changed.
            stale = page.request.post(f"{DATA_SOURCES}/priority", form={
                "form_token": page.get_attribute("input[name=form_token]", "value"),
                "data_type": "meridian.v1.Price", "kind": "close", "datasets": "alpaca-1:daily",
                "against_updated_at_ns": "1791417600000000000", "note": "Read before."},
                max_redirects=0)
            check(stale.status == 400 and "changed since it was read" in stale.text(),
                  f"a priority sent against an older read is refused as changed: {stale.status}")
        check(not errors, f"the Data sources page at {size} ran without a script error: {errors}")
        ctx.close()


def the_icon_is_linked_and_served(browser):
    """The product owner, 2026-10-06: the Open Meridian icon on the
    deployment too."""
    ctx = context(browser)
    page = ctx.new_page()
    page.goto(ADMIN_PAGE)
    settled(page)
    links = page.evaluate("""async () => Promise.all(
      [...document.head.querySelectorAll('link[rel~=icon], link[rel=apple-touch-icon]')].map(
        async (l) => { const r = await fetch(l.href);
          return [l.rel, new URL(l.href).pathname, r.status, r.headers.get('content-type')]; }))""")
    check([path for _, path, _, _ in links] == ["/favicon-32.png", "/favicon.svg", "/apple-touch-icon.png"],
          f"the head links the PNG, then the SVG, then the touch icon: {links}")
    check(all(status == 200 for _, _, status, _ in links)
          and [kind for _, _, _, kind in links] == ["image/png", "image/svg+xml", "image/png"],
          f"each is answered with its type: {links}")
    ico = page.request.get(f"{BASE}/favicon.ico")
    check(ico.status == 200 and ico.headers.get("content-type") == "image/x-icon",
          f"/favicon.ico is answered unprompted: {ico.status} {ico.headers.get('content-type')}")
    ctx.close()
    for scheme in ("light", "dark"):
        shown = browser.new_context(viewport={"width": 160, "height": 160}, color_scheme=scheme)
        icon = shown.new_page()
        icon.set_content(f'<body style="margin:0;display:grid;place-items:center;height:100vh;'
                         f'background:{"#fff" if scheme == "light" else "#14161f"}">'
                         f'<img src="{BASE}/favicon.svg" width="128" height="128"></body>')
        icon.wait_for_timeout(300)
        drawn = icon.evaluate("() => document.querySelector('img').naturalWidth > 0")
        check(drawn, f"the SVG icon is drawn in a {scheme} browser")
        icon.screenshot(path=os.path.join(OUT, f"favicon-{scheme}.png"))
        shown.close()


# Contract v24's pages and the folded fix, at the sizes the build plan names
# (375x812, 582x1432, 1280x800): each with no horizontal scroll, how far it
# scrolls down recorded, the kit's check said, and a screenshot of each.
V24_SIZES = [("375x812", 375, 812), ("582x1432", 582, 1432), ("1280x800", 1280, 800)]
MEASURE = """() => ({
  wide: document.documentElement.scrollWidth - innerWidth,
  tall: document.documentElement.scrollHeight - innerHeight,
})"""


def measured(page, label, shot):
    m = page.evaluate(MEASURE)
    problems = page.evaluate(FIT)
    print(f"MEASURE {label}: horizontal={m['wide']} vertical={m['tall']} "
          f"fit={'ok' if not problems else '; '.join(problems)}", flush=True)
    check(m["wide"] <= 0, f"{label} scrolls no wider than the screen: {m['wide']}px over")
    page.screenshot(path=os.path.join(OUT, shot), full_page=True)
    return problems


def the_v24_pages(browser, prefix):
    """Contract v24: the Instruments page's provisional records and the
    usage report's setting; the home page's counts; the escape hatch's
    setting; the account Close's dialog (approved 2026-10-14); and a
    plugin's Summary of five figures at phone width (the build plan's O8)."""
    for size, width, height in V24_SIZES:
        ctx = context(browser, width, height)
        page = ctx.new_page()
        errors = []
        page.on("pageerror", lambda e: errors.append(str(e)))

        page.goto(f"{BASE}/admin/instruments#provisional")
        settled(page)
        rows = page.eval_on_selector_all("#provisional-table tbody tr", "rows => rows.map(r => r.dataset.reason)")
        check(len(rows) == 4, f"at {size} the provisional records are listed, each with why: {rows}")
        # Its text whole: the kit's pager hides the rows past a screen.
        why = page.evaluate("() => document.querySelector('#provisional').textContent")
        check("with the platform's staff" in why and "class needed" in why and "Matched: 118" in " ".join(why.split()),
              f"at {size} their reasons and the migration's counts are said")
        measured(page, f"Instruments, provisional, at {size}", f"v24-instruments-provisional-{size}.png")
        page.goto(f"{BASE}/admin/instruments#licensed")
        settled(page)
        check(page.locator("form#usage-form select[name=reports_usage]").count() == 1,
              f"at {size} Report identifier usage is set at the Instruments page")
        measured(page, f"Instruments, usage report, at {size}", f"v24-instruments-usage-{size}.png")

        page.goto(f"{BASE}/")
        settled(page)
        provisional = page.get_attribute("#provisional-records", "data-provisional")
        orders = page.get_attribute("#local-record-orders", "data-open-order-count")
        check(provisional == "4" and orders == "3",
              f"at {size} the home page counts the provisional records and the orders on local records: {provisional}, {orders}")
        measured(page, f"Home, at {size}", f"v24-home-{size}.png")

        page.goto(f"{BASE}/admin/trading-instructions#local-records")
        settled(page)
        said = page.get_attribute("[data-allows-local-record-orders]", "data-allows-local-record-orders")
        check(said in ("true", "false"), f"at {size} the escape hatch's setting is drawn: {said}")
        measured(page, f"Trading instructions, local records, at {size}", f"v24-local-records-{size}.png")

        page.goto(ADMIN_PAGE + "#accounts")
        settled(page)
        for name, offers in (("Joint taxable", True), ("Credit Suisse 401(k)", False)):
            page.click(f"#accounts-table tr[data-name='{name}'] button[data-attached]")
            page.wait_for_timeout(300)
            items = page.eval_on_selector_all("dialog[open] [data-attached-list] li a", "as => as.map(a => a.getAttribute('href'))")
            buttons = page.eval_on_selector_all("dialog[open] .dialog-foot button", "bs => bs.map(b => b.textContent.trim())")
            check(items and all(href.startswith("/admin/plugins/") for href in items),
                  f"at {size} Close on {name} lists what is attached, each linking to its plugin's page: {items}")
            check(("Unlink all and close" in buttons) == offers and "Cancel" in buttons,
                  f"at {size} Close on {name} offers {buttons}")
            boxed = inside_the_viewport(page)
            check(not boxed, f"at {size} the Close dialog for {name} stays within the viewport{': ' + boxed if boxed else ''}")
            spilled = page.evaluate(INSIDE)
            check(not spilled, f"at {size} nothing in the Close dialog for {name} reaches past it"
                  f"{': ' + '; '.join(spilled[:4]) if spilled else ''}")
            m = page.evaluate(MEASURE)
            print(f"MEASURE Close dialog, {name}, at {size}: horizontal={m['wide']} vertical={m['tall']}", flush=True)
            page.screenshot(path=os.path.join(OUT, f"v24-close-{'granted' if offers else 'attached'}-{size}.png"))
            page.click("dialog[open] [data-dialog-close]")
            page.wait_for_timeout(200)
        confirm = page.get_attribute("#accounts-table tr[data-name='Kalshi'] form[data-confirm]", "data-confirm")
        check(bool(confirm), f"at {size} an account attached to nothing asks the usual confirmation: {confirm!r}")

        page.goto(SUMMARY)
        settled(page)
        figures = page.eval_on_selector_all("section.figures .figure", "fs => fs.length")
        check(figures == 5, f"at {size} the Summary draws its five figures: {figures}")
        spill = page.evaluate("""() => [...document.querySelectorAll('section.figures .figure')]
          .filter((f) => f.scrollWidth > f.clientWidth + 1 || f.getBoundingClientRect().right > innerWidth + 0.5)
          .map((f) => f.dataset.figure)""")
        check(not spill, f"at {size} no figure overflows its tile or the screen: {spill}")
        measured(page, f"Summary of five figures, at {size}", f"v24-summary-{size}.png")

        check(not errors, f"the v24 pages at {size} ran without a script error: {errors}")
        ctx.close()
    if PASS != "fit":
        ctx = context(browser)
        page = ctx.new_page()
        page.goto(f"{BASE}/admin/trading-instructions#local-records")
        settled(page)
        page.fill("form#local-records-form [name=note]", "orders only on the platform's records")
        page.click("form#local-records-form button.primary")
        settled(page)
        said = page.get_attribute("[data-allows-local-record-orders]", "data-allows-local-record-orders")
        check(said == "false", f"turned off from its form, the page says so: {said}")
        ctx.close()


with sync_playwright() as playwright:
    browser = playwright.chromium.launch()
    prefix = "development" if PASS == "fit" else "settings"
    if PASS != "fit":
        the_icon_is_linked_and_served(browser)
        the_grid_adds_rows_past_four_and_posts_them(browser)
        two_hundred_are_accepted_and_two_hundred_and_one_refused(browser)
        without_script_the_plain_table_posts(browser)
    the_access_editor_has_a_row_per_role(browser, prefix)
    the_raw_records_and_the_holds(browser, prefix)
    the_data_sources_page(browser, prefix)
    the_accounts_tab_holds_the_book(browser, prefix)
    the_service_accounts_tab_and_runs_as(browser, prefix)
    the_connectivity_part_and_the_calendars_tab(browser, prefix)
    for name, url in [
        ("area-settings", AREA + "settings"),
        ("area-plan-code-links", AREA + PLAN),
        ("area-cash-links", AREA + CASH),
        ("portal-settings", PORTAL + "settings"),
        ("portal-plan-code-links", PORTAL + PLAN),
        ("portal-cash-links", PORTAL + CASH),
    ]:
        fits(browser, name, url, prefix)
    a_secret_says_where_it_stands(browser, prefix)
    the_v24_pages(browser, prefix)
    browser.close()

if failures:
    print(f"{len(failures)} check(s) failed", flush=True)
    sys.exit(1)
print("settings pages: every check held", flush=True)
