//! Bounded webview, accordion and read-only setup checks. No prompts or input hooks run.

use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

use tauri::{Manager, WebviewWindow};

const PENDING: u8 = 0;
const PASSED: u8 = 1;
const FAILED: u8 = 2;
static OUTCOME: AtomicU8 = AtomicU8::new(PENDING);

pub fn exit_code() -> i32 {
    i32::from(OUTCOME.load(Ordering::Acquire) != PASSED)
}

fn finish(app: &tauri::AppHandle, passed: bool, message: &str) {
    let outcome = if passed { PASSED } else { FAILED };
    if OUTCOME
        .compare_exchange(PENDING, outcome, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        println!("{message}");
        app.exit(exit_code());
    }
}

const RENDER_CHECK: &str = r#"(() => {
  const heading = document.querySelector('#permissions-title');
  const button = document.querySelector('#permissions-refresh');
  const sections = [...document.querySelectorAll('.setup-section')];
  const locked = sections.filter(section => section.dataset.section !== 'ready');
  const shell = document.querySelector('.shell');
  const header = document.querySelector('.window-header');
  const visible = node => node && node.getClientRects().length > 0
    && getComputedStyle(node).visibility === 'visible';
  const inViewport = node => {
    if (!visible(node)) return false;
    const bounds = node.getBoundingClientRect();
    return bounds.top >= 0 && bounds.left >= 0
      && bounds.bottom <= innerHeight + 1 && bounds.right <= innerWidth + 1;
  };
  const theme = document.documentElement.dataset.theme;
  const dark = theme === 'dark' || (theme !== 'light' && matchMedia('(prefers-color-scheme: dark)').matches);
  const scheme = getComputedStyle(document.documentElement).colorScheme;
  const color = getComputedStyle(document.body).color.match(/\d+/g)?.slice(0, 3).map(Number);
  const background = getComputedStyle(document.body).backgroundColor.match(/\d+/g)?.slice(0, 3).map(Number);
  return [
    document.readyState === 'complete' && Math.abs(innerWidth - EXPECTED_WIDTH) < 1
      && innerHeight > 0 && innerHeight <= MAXIMUM_HEIGHT,
    visible(heading) && heading.textContent.trim().length > 0 && inViewport(header)
      && header.getBoundingClientRect().top === 0
      && document.documentElement.dataset.platform === window.__MONHOP_PLATFORM__,
    inViewport(shell) && document.documentElement.scrollWidth <= innerWidth + 1
      && document.documentElement.scrollHeight <= innerHeight + 1
      && (window.__MONHOP_PLATFORM__ !== 'macos' || document.querySelector('.window-heading').getBoundingClientRect().left >= 82)
      && (window.__MONHOP_PLATFORM__ !== 'windows'
        || [...document.querySelectorAll('[data-window-action]')].length === 3
          && [...document.querySelectorAll('[data-window-action]')].every(inViewport)),
    parseFloat(getComputedStyle(heading).fontSize) <= 30
      && parseFloat(getComputedStyle(document.body).fontSize) <= 14
      && button?.getBoundingClientRect().height >= 28
      && button?.getBoundingClientRect().height <= 40,
    document.querySelector('#native-state').dataset.checkState === 'unchecked',
    typeof window.__TAURI__?.core?.invoke === 'function',
    sections.length === 3 && !document.querySelector('#setup-view').hidden
      && document.querySelector('#home-view').hidden
      && inViewport(sections[0].querySelector('.section-header'))
      && inViewport(button) && button.disabled === false,
    locked.length === 2
      && locked.every(section => section.dataset.locked === 'true'
        && section.querySelector('.section-body').inert)
      && document.querySelector('#sharing-source-platform') === null
      && !document.querySelector('#sharing-open-trial:not(:disabled)'),
    (theme === undefined ? scheme === 'light dark' : scheme === theme)
      && color?.length === 3 && background?.length === 3
      && (dark ? background.every(value => value < 64) && color.every(value => value > 190)
        : background.every(value => value > 190) && color.every(value => value < 64))
  ].map(value => value ? '1' : '0').join('');
})()"#;

fn render_flags(value: &str) -> Option<[bool; 9]> {
    let bytes = value.as_bytes();
    if bytes.len() != 11
        || bytes[0] != b'"'
        || bytes[10] != b'"'
        || !bytes[1..10]
            .iter()
            .all(|value| matches!(value, b'0' | b'1'))
    {
        return None;
    }
    Some(std::array::from_fn(|index| bytes[index + 1] == b'1'))
}

pub fn arm_timeout(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        finish(&app, false, "MonHop UI check: FAIL (render deadline).");
    });
}

pub fn check(window: WebviewWindow) {
    // Only the explicit diagnostic requests focus before sampling animation frames.
    if window.set_focus().is_err() {
        finish(
            window.app_handle(),
            false,
            "MonHop UI check: FAIL (window focus).",
        );
        return;
    }
    check_at_width(window, 980);
}

fn check_at_width(window: WebviewWindow, width: u16) {
    if OUTCOME.load(Ordering::Acquire) != PENDING {
        return;
    }
    let retry_window = window.clone();
    let app = window.app_handle().clone();
    let callback_app = app.clone();
    let maximum_height = if width == 980 { 720 } else { 560 };
    let script = RENDER_CHECK
        .replace("EXPECTED_WIDTH", &width.to_string())
        .replace("MAXIMUM_HEIGHT", &maximum_height.to_string());
    if window
        .eval_with_callback(script, move |value| {
            match render_flags(&value) {
                Some(flags) => {
                    // Wait for module loading or the requested resize within the original deadline.
                    if !flags[0] {
                        let retry_window = retry_window.clone();
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(50));
                            check_at_width(retry_window, width);
                        });
                        return;
                    }
                    println!("MonHop UI categories: width={} maximum_height={} ready={} html={} layout={} density={} script={} bridge={} controls={} sharing_disabled={} theme={}", width, maximum_height, flags[0], flags[1], flags[2], flags[3], flags[4], flags[5], flags[6], flags[7], flags[8]);
                    if flags.into_iter().all(|flag| flag) {
                        check_accordion(retry_window.clone(), width);
                    } else {
                        finish(&callback_app, false, "MonHop UI check: FAIL (rendered content or startup controls).");
                    }
                }
                None => finish(&callback_app, false, "MonHop UI check: FAIL (invalid render receipt)."),
            }
        })
        .is_err()
    {
        finish(&app, false, "MonHop UI check: FAIL (webview evaluation).");
    }
}

const ACCORDION_CHECK: &str = r#"(() => {
  window.__MONHOP_ACCORDION_CHECK__ = 'pending:opening';
  (async () => {
    const item = document.querySelector('[data-disclosure="overview-details"]');
    const trigger = item?.querySelector('.accordion-trigger');
    const panel = item?.querySelector('.accordion-panel');
    const frame = () => new Promise(requestAnimationFrame);
    const settle = async () => {
      while (item.getAnimations({ subtree: true }).some(a => a.playState === 'running')) await frame();
    };
    const midpoint = async () => {
      const animation = panel.getAnimations().find(a => a.playState === 'running');
      if (matchMedia('(prefers-reduced-motion: reduce)').matches) return !animation;
      if (!animation) return false;
      animation.pause();
      animation.currentTime = animation.effect.getTiming().duration / 2;
      await frame();
      const height = panel.getBoundingClientRect().height;
      const animated = height > 0 && height < panel.scrollHeight - 1;
      animation.play();
      return animated;
    };
    await Promise.all(document.querySelector('#setup-view').getAnimations({ subtree: true }).map(a => a.finished.catch(() => {})));
    const flags = [trigger?.tagName === 'BUTTON' && panel?.getAttribute('role') === 'region'
      && trigger.getAttribute('aria-controls') === panel.id
      && panel.getAttribute('aria-labelledby') === trigger.id];
    trigger.click();
    flags.push(await midpoint());
    window.__MONHOP_ACCORDION_CHECK__ = 'pending:expanded';
    await settle();
    const copy = document.querySelector('.overview-copy').getBoundingClientRect();
    const row = trigger.getBoundingClientRect();
    const body = panel.getBoundingClientRect();
    flags.push(row.top >= copy.bottom && body.top >= row.bottom - 1
      && Math.abs(row.width - body.width) < 2);
    flags.push(trigger.getAttribute('aria-expanded') === 'true' && !panel.hidden && !panel.inert
      && body.height >= panel.scrollHeight - 1);
    trigger.click();
    window.__MONHOP_ACCORDION_CHECK__ = 'pending:closing';
    flags.push(await midpoint());
    await settle();
    flags.push(trigger.getAttribute('aria-expanded') === 'false' && panel.hidden && panel.inert);
    window.__MONHOP_ACCORDION_CHECK__ = 'pending:reversal';
    trigger.click(); await frame(); trigger.click(); await frame(); trigger.click();
    await settle();
    flags.push(trigger.getAttribute('aria-expanded') === 'true' && !panel.hidden && !panel.inert
      && panel.getBoundingClientRect().height >= panel.scrollHeight - 1);
    flags.push(document.querySelector('.content-region').scrollWidth <= document.querySelector('.content-region').clientWidth + 1);
    window.__MONHOP_ACCORDION_CHECK__ = 'pending:finishing';
    trigger.click(); await settle();
    flags.push(panel.hidden && panel.inert
      && document.querySelector('#native-state').dataset.checkState === 'unchecked'
      && [...document.querySelectorAll('.setup-section[data-locked="true"]')].length === 2
      && document.querySelector('#sharing-source-platform') === null
      && !document.querySelector('#sharing-open-trial:not(:disabled)'));
    window.__MONHOP_ACCORDION_CHECK__ = flags.map(v => v ? '1' : '0').join('');
  })().catch(() => { window.__MONHOP_ACCORDION_CHECK__ = '000000000'; });
  return 'started';
})()"#;

fn check_accordion(window: WebviewWindow, width: u16) {
    let app = window.app_handle().clone();
    let callback_window = window.clone();
    if window
        .eval_with_callback(ACCORDION_CHECK, move |value| {
            if value == "\"started\"" {
                poll_accordion(callback_window.clone(), width, String::new());
            } else {
                finish(
                    callback_window.app_handle(),
                    false,
                    "MonHop accordion check: FAIL (start).",
                );
            }
        })
        .is_err()
    {
        finish(&app, false, "MonHop accordion check: FAIL (evaluation).");
    }
}

fn poll_accordion(window: WebviewWindow, width: u16, previous: String) {
    if OUTCOME.load(Ordering::Acquire) != PENDING {
        return;
    }
    let app = window.app_handle().clone();
    let callback_window = window.clone();
    if window
        .eval_with_callback("window.__MONHOP_ACCORDION_CHECK__", move |value| {
            let app = callback_window.app_handle();
            if value.starts_with("\"pending:") {
                if value != previous {
                    println!("MonHop accordion progress: width={width} {value}");
                }
                let callback_window = callback_window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    poll_accordion(callback_window, width, value);
                });
                return;
            }
            let Some(flags) = render_flags(&value) else {
                finish(app, false, "MonHop accordion check: FAIL (invalid receipt).");
                return;
            };
            println!("MonHop accordion categories: width={} semantics={} opening={} stacked={} expanded={} closing={} collapsed={} reversal={} layout={} setup_unchanged={}", width, flags[0], flags[1], flags[2], flags[3], flags[4], flags[5], flags[6], flags[7], flags[8]);
            if !flags.into_iter().all(|flag| flag) {
                finish(app, false, "MonHop accordion check: FAIL (behavior).");
            } else if width == 980 {
                if callback_window.set_size(tauri::LogicalSize::new(760.0, 560.0)).is_err() {
                    finish(app, false, "MonHop UI check: FAIL (minimum window resize).");
                } else {
                    check_at_width(callback_window.clone(), 760);
                }
            } else {
                check_readonly_status(callback_window.clone());
            }
        })
        .is_err()
    {
        finish(&app, false, "MonHop accordion check: FAIL (receipt evaluation).");
    }
}

fn check_readonly_status(window: WebviewWindow) {
    let app = window.app_handle().clone();
    let next = window.clone();
    if window
        .eval_with_callback(
            r#"(() => {
        const button = document.querySelector('#permissions-refresh');
        if (!button || button.disabled) return 'failed';
        // The diagnostic leaves logging off, so the real command proves ACL access without opening a folder.
        window.__MONHOP_LOG_CHECK__ = 'pending';
        window.__TAURI__.core.invoke('reveal_logs').then(
            () => { window.__MONHOP_LOG_CHECK__ = 'failed'; },
            error => { window.__MONHOP_LOG_CHECK__ = error === 'Logging is not active in this run.' ? 'passed' : 'failed'; }
        );
        button.click();
        return 'started';
    })()"#,
            move |value| {
                if value == "\"started\"" {
                    poll_readonly_status(next.clone());
                } else {
                    finish(
                        next.app_handle(),
                        false,
                        "MonHop UI check: FAIL (read-only refresh control).",
                    );
                }
            },
        )
        .is_err()
    {
        finish(
            &app,
            false,
            "MonHop UI check: FAIL (read-only check evaluation).",
        );
    }
}

fn poll_readonly_status(window: WebviewWindow) {
    if OUTCOME.load(Ordering::Acquire) != PENDING {
        return;
    }
    let app = window.app_handle().clone();
    let next = window.clone();
    if window.eval_with_callback(r#"(() => {
        const button = document.querySelector('#permissions-refresh');
        if (!button) return 'failed';
        const logCheck = window.__MONHOP_LOG_CHECK__;
        if (button.disabled || logCheck === 'pending') return 'pending';
        return logCheck === 'passed'
            && document.querySelector('#native-state').dataset.checkState === 'checked'
            && document.querySelector('#permissions-content').innerText.length > 30
            && document.querySelector('#interface-content').innerText.length > 30
            && document.querySelector('#sharing-source-platform') === null
            && !document.querySelector('#sharing-open-trial:not(:disabled)') ? 'passed' : 'failed';
    })()"#, move |value| {
        if value == "\"pending\"" {
            let next = next.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                poll_readonly_status(next);
            });
            return;
        }
        let app = next.app_handle();
        if value != "\"passed\"" {
            finish(app, false, "MonHop UI check: FAIL (read-only status rendering).");
            return;
        }
        println!("MonHop setup categories: readonly_status_rendered=true sharing_disabled=true reveal_logs_authorized=true");
        let tray_passed = crate::tray::is_installed(app)
            && crate::tray::hide_main_window(app).is_ok()
            && next.is_visible().is_ok_and(|visible| !visible)
            && crate::tray::show_main_window(app).is_ok()
            && next.is_visible().unwrap_or(false);
        println!("MonHop tray categories: installed_hide_show={tray_passed}");
        finish(app, tray_passed, if tray_passed {
            "MonHop UI check: PASS (guided setup, animated accordions, read-only status and tray hide/show; sharing disabled)."
        } else {
            "MonHop UI check: FAIL (native tray hide/show)."
        });
    }).is_err() {
        finish(&app, false, "MonHop UI check: FAIL (read-only receipt evaluation).");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_receipt_requires_nine_boolean_categories() {
        assert_eq!(render_flags("\"111111111\""), Some([true; 9]));
        assert_eq!(
            render_flags("\"010101010\""),
            Some([false, true, false, true, false, true, false, true, false])
        );
    }

    #[test]
    fn malformed_render_receipts_are_rejected() {
        for value in [
            "",
            "true",
            "null",
            "{}",
            "111111111",
            "\"11111111\"",
            "\"1111111111\"",
            "\"11111111x\"",
            " \"111111111\"",
            "\"111111111\"\n",
        ] {
            assert_eq!(render_flags(value), None);
        }
    }
}
