//! Choosing many among many (the product owner, 2026-09-30: "should have
//! search boxes"): accounts into an account group, people into a user group,
//! plugins into an access group.
//!
//! The markup is a plain list of checkboxes in a fieldset, which is what the
//! form was before and what it stays without script. With script (the admin
//! page's, [`SCRIPT`]), a search box narrows the list as it is typed, the
//! chosen show at the top as chips that remove them, and "Select all shown"
//! and "Clear" act on the list. Every option is in the page once, however
//! many forms there are: the admin page has one dialog per kind of record,
//! which an Edit fills, so the page grows with the options and not with the
//! options times the records.

use std::collections::HashSet;

use crate::html::escape;

/// One option.
#[derive(Default)]
pub struct Choice {
    pub value: String,
    /// What it is called.
    pub label: String,
    /// Small beside it, as an identifier is.
    pub detail: String,
    /// More words it is found by, not shown.
    pub also: String,
    /// Listed only while it is chosen, as a closed account is.
    pub only_when_chosen: bool,
    /// Markup after it, already HTML: an access entry's level.
    pub after: String,
}

/// A picker named `id`, posting each chosen value as `name`.
pub fn many(
    id: &str,
    name: &str,
    legend: &str,
    noun: &str,
    choices: &[Choice],
    chosen: &HashSet<&str>,
) -> String {
    let options: String = choices
        .iter()
        .map(|choice| {
            let detail = if choice.detail.is_empty() || choice.detail == choice.label {
                String::new()
            } else {
                format!(" <span class=\"id\">{}</span>", escape(&choice.detail))
            };
            format!(
                "<div class=\"picker-option\"{also}{only}><label class=\"check\"><input type=\"checkbox\" \
                 name=\"{name}\" value=\"{value}\"{checked}> <span class=\"option-label\">{label}</span>{detail}\
                 </label>{after}</div>",
                also = if choice.also.is_empty() {
                    String::new()
                } else {
                    format!(" data-also=\"{}\"", escape(&choice.also))
                },
                only = if choice.only_when_chosen {
                    " data-only-when-chosen"
                } else {
                    ""
                },
                name = escape(name),
                value = escape(&choice.value),
                checked = if chosen.contains(choice.value.as_str()) {
                    " checked"
                } else {
                    ""
                },
                label = escape(&choice.label),
                after = choice.after,
            )
        })
        .collect();
    let id = escape(id);
    let noun = escape(noun);
    format!(
        "<fieldset class=\"checks picker\" id=\"{id}\" data-picker><legend>{legend}</legend>\
         <div class=\"picker-tools\" hidden><input type=\"search\" placeholder=\"Search {noun}\" \
         aria-label=\"Search {noun}\" aria-controls=\"{id}-options\" data-picker-search>\
         <button type=\"button\" data-picker-all>Select all shown</button>\
         <button type=\"button\" data-picker-clear>Clear</button></div>\
         <p class=\"picker-status\" aria-live=\"polite\" hidden></p>\
         <ul class=\"picker-chosen\" aria-label=\"Chosen {noun}\" hidden></ul>\
         <div class=\"picker-options\" id=\"{id}-options\">{options}</div>\
         <p class=\"picker-none\" hidden>No {noun} match that search.</p></fieldset>",
        legend = escape(legend),
    )
}

/// The pickers' script, for the page that holds them: each `[data-picker]`
/// gets its search box, chips and buttons, and `refresh()` for a dialog that
/// has just filled it. Each option's text is read once; a keystroke only
/// flips the options whose state changes, at most once a frame.
pub const SCRIPT: &str = r#"
  var MOST_CHIPS = 12;
  function picker(box) {
    var tools = box.querySelector(".picker-tools");
    var search = box.querySelector("[data-picker-search]");
    var status = box.querySelector(".picker-status");
    var chips = box.querySelector(".picker-chosen");
    var none = box.querySelector(".picker-none");
    var items = Array.prototype.slice.call(box.querySelectorAll(".picker-option"));
    var boxes = items.map(function (item) { return item.querySelector("input[type=checkbox]"); });
    var names = items.map(function (item) { return item.querySelector(".option-label").textContent; });
    var texts = items.map(function (item) { return (item.textContent + " " + (item.getAttribute("data-also") || "")).toLowerCase(); });
    var only = items.map(function (item) { return item.hasAttribute("data-only-when-chosen"); });
    var pending = false;
    tools.hidden = false;
    status.hidden = false;
    chips.hidden = false;
    function narrow() {
      pending = false;
      var words = search.value.toLowerCase().split(/\s+/).filter(Boolean);
      var shown = 0, chosen = 0, listed = 0;
      for (var i = 0; i < items.length; i++) {
        var hide = only[i] && !boxes[i].checked;
        if (!hide) listed++;
        if (!hide && words.length) hide = !words.every(function (w) { return texts[i].indexOf(w) !== -1; });
        if (items[i].hidden !== hide) items[i].hidden = hide;
        if (!hide) shown++;
        if (boxes[i].checked) chosen++;
      }
      none.hidden = shown !== 0 || !words.length;
      status.textContent = chosen + " chosen" + (words.length ? ", " + shown + " of " + listed + " shown" : ", " + listed + " in all");
    }
    function chosenChips() {
      var picked = [];
      for (var i = 0; i < items.length; i++) if (boxes[i].checked) picked.push(i);
      var nodes = picked.slice(0, MOST_CHIPS).map(function (i) {
        var li = document.createElement("li");
        var button = document.createElement("button");
        button.type = "button";
        button.textContent = names[i] + " \u00d7";
        button.setAttribute("aria-label", "Remove " + names[i]);
        button.addEventListener("click", function () { boxes[i].checked = false; refresh(); boxes[i].focus(); });
        li.appendChild(button);
        return li;
      });
      if (picked.length > MOST_CHIPS) {
        var more = document.createElement("li");
        more.className = "more";
        more.textContent = "and " + (picked.length - MOST_CHIPS) + " more";
        nodes.push(more);
      }
      chips.replaceChildren.apply(chips, nodes);
    }
    function refresh() { narrow(); chosenChips(); }
    search.addEventListener("input", function () {
      if (pending) return;
      pending = true;
      window.requestAnimationFrame(narrow);
    });
    // Enter in the search box narrows; it never sends the form.
    search.addEventListener("keydown", function (event) { if (event.key === "Enter") event.preventDefault(); });
    box.addEventListener("change", function (event) { if (event.target.type === "checkbox") refresh(); });
    box.querySelector("[data-picker-all]").addEventListener("click", function () {
      for (var i = 0; i < items.length; i++) if (!items[i].hidden) boxes[i].checked = true;
      refresh();
    });
    box.querySelector("[data-picker-clear]").addEventListener("click", function () {
      for (var i = 0; i < items.length; i++) boxes[i].checked = false;
      refresh();
    });
    box.refresh = function () { search.value = ""; refresh(); };
    refresh();
  }
  Array.prototype.forEach.call(document.querySelectorAll("[data-picker]"), picker);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(value: &str, label: &str) -> Choice {
        Choice {
            value: value.into(),
            label: label.into(),
            detail: value.into(),
            ..Default::default()
        }
    }

    #[test]
    fn without_script_a_picker_is_the_plain_list_of_checkboxes() {
        let chosen: HashSet<&str> = ["ACC-2"].into();
        let html = many(
            "account-picker",
            "account_ids",
            "Accounts",
            "accounts",
            &[choice("ACC-1", "Growth <A>"), choice("ACC-2", "Income")],
            &chosen,
        );
        assert!(html.starts_with(
            "<fieldset class=\"checks picker\" id=\"account-picker\" data-picker><legend>Accounts</legend>"
        ));
        // Everything the script adds is hidden until it runs.
        for part in [
            "<div class=\"picker-tools\" hidden>",
            "<p class=\"picker-status\" aria-live=\"polite\" hidden></p>",
            "<ul class=\"picker-chosen\" aria-label=\"Chosen accounts\" hidden></ul>",
            "<p class=\"picker-none\" hidden>",
        ] {
            assert!(html.contains(part), "{part}");
        }
        assert!(html.contains(
            "<div class=\"picker-option\"><label class=\"check\"><input type=\"checkbox\" name=\"account_ids\" \
             value=\"ACC-1\"> <span class=\"option-label\">Growth &lt;A&gt;</span> <span class=\"id\">ACC-1</span>\
             </label></div>"
        ), "{html}");
        assert!(html.contains("value=\"ACC-2\" checked>"));
        // Labelled for a screen reader: the search, and the chosen.
        assert!(html
            .contains("aria-label=\"Search accounts\" aria-controls=\"account-picker-options\""));
        assert_eq!(html.matches("type=\"checkbox\"").count(), 2);
    }

    #[test]
    fn the_script_reads_each_option_once_and_flips_only_what_changes() {
        for held in [
            "var texts = items.map(",
            "if (items[i].hidden !== hide) items[i].hidden = hide;",
            "window.requestAnimationFrame(narrow);",
            "if (event.key === \"Enter\") event.preventDefault();",
            "for (var i = 0; i < items.length; i++) if (!items[i].hidden) boxes[i].checked = true;",
            "button.setAttribute(\"aria-label\", \"Remove \" + names[i]);",
            "button.textContent = names[i] + \" \\u00d7\";",
            "var MOST_CHIPS = 12;",
        ] {
            assert!(SCRIPT.contains(held), "{held}");
        }
        assert!(
            !SCRIPT.contains("innerHTML"),
            "a name is text, never markup"
        );
    }
}
