use super::hosted::{is_callback, provider_page};
use block2::Block;
use objc2::{
    DeclaredClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::{NSObject, NSObjectProtocol, ProtocolObject, Sel},
};
use objc2_foundation::NSURL;
use objc2_web_kit::{
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKWebView,
};
use std::{ffi::c_void, time::Duration};
use tauri::WebviewWindow;
use tokio::sync::oneshot;
use url::Url;

type NavigationDelegate = ProtocolObject<dyn WKNavigationDelegate>;

struct NavigationPolicy {
    original: Weak<NavigationDelegate>,
    callback: Url,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavigationPolicy]
    struct HostedNavigationDelegate;

    unsafe impl NSObjectProtocol for HostedNavigationDelegate {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to_selector(&self, selector: Sel) -> bool {
            let responds: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            responds || self.ivars().original.load()
                .is_some_and(|original| original.respondsToSelector(selector))
        }
    }

    impl HostedNavigationDelegate {
        #[unsafe(method_id(forwardingTargetForSelector:))]
        fn forwarding_target(&self, selector: Sel) -> Option<Retained<NavigationDelegate>> {
            self.ivars().original.load().filter(|original| original.respondsToSelector(selector))
        }
    }

    unsafe impl WKNavigationDelegate for HostedNavigationDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_navigation(
            &self,
            webview: &WKWebView,
            action: &WKNavigationAction,
            decide: &Block<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            let requested = unsafe { action.request() }.URL().and_then(|url| parse(&url));
            let frame = unsafe { action.targetFrame() }.map(|frame| unsafe { frame.isMainFrame() });
            let download = unsafe { action.shouldPerformDownload() };

            if download || requested.as_ref().is_some_and(|url| {
                frame != Some(true) && is_callback(url, &self.ivars().callback)
            }) {
                decide.call((WKNavigationActionPolicy::Cancel,));
            } else if frame == Some(false)
                && requested.as_ref().is_some_and(challenge_page)
                && unsafe { webview.URL() }.and_then(|url| parse(&url))
                    .as_ref().is_some_and(provider_page)
            {
                decide.call((WKNavigationActionPolicy::Allow,));
            } else if let Some(original) = self.ivars().original.load() {
                unsafe { original.webView_decidePolicyForNavigationAction_decisionHandler(webview, action, decide) };
            } else {
                decide.call((WKNavigationActionPolicy::Cancel,));
            }
        }
    }
);

fn parse(url: &NSURL) -> Option<Url> {
    Url::parse(&url.absoluteString()?.to_string()).ok()
}

fn challenge_page(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("www.google.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.path(),
            "/recaptcha/api2/anchor"
                | "/recaptcha/api2/bframe"
                | "/recaptcha/api/fallback"
                | "/recaptcha/enterprise/anchor"
                | "/recaptcha/enterprise/bframe"
        )
}

pub(super) async fn install(window: &WebviewWindow, callback: Url) -> Result<(), ()> {
    let (ready, installed) = oneshot::channel();
    window
        .with_webview(move |webview| {
            let result = install_delegate(webview.inner(), callback);
            let _ = ready.send(result);
        })
        .map_err(|_| ())?;
    tokio::time::timeout(Duration::from_secs(5), installed)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?
}

fn install_delegate(webview: *mut c_void, callback: Url) -> Result<(), ()> {
    let main = MainThreadMarker::new().ok_or(())?;
    // Tauri supplies the live WKWebView pointer on the main thread.
    let view = unsafe { webview.cast::<WKWebView>().as_ref() }.ok_or(())?;
    let original = unsafe { view.navigationDelegate() }.ok_or(())?;
    let delegate = main
        .alloc::<HostedNavigationDelegate>()
        .set_ivars(NavigationPolicy {
            original: Weak::from_retained(&original),
            callback,
        });
    let delegate: Retained<HostedNavigationDelegate> = unsafe { msg_send![super(delegate), init] };
    static ASSOCIATION_KEY: u8 = 0;
    unsafe {
        // WKWebView holds a weak delegate. Wry owns the original, so use it to
        // retain the wrapper for this window without creating a cycle.
        objc2::ffi::objc_setAssociatedObject(
            Retained::as_ptr(&original).cast_mut().cast(),
            std::ptr::from_ref(&ASSOCIATION_KEY).cast(),
            Retained::as_ptr(&delegate).cast_mut().cast(),
            objc2::ffi::OBJC_ASSOCIATION_RETAIN_NONATOMIC,
        );
        view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    }
    Ok(())
}
