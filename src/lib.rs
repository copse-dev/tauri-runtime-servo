// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! The Servo Tauri [`Runtime`].
//!
//! None of the exposed API of this crate is stable, and it may break semver
//! compatibility in the future. The major version only signifies the intended Tauri version.

#![doc(
  html_logo_url = "https://github.com/tauri-apps/tauri/raw/dev/.github/icon.png",
  html_favicon_url = "https://github.com/tauri-apps/tauri/raw/dev/.github/icon.png"
)]

#[cfg(not(desktop))]
compile_error!("the experimental Servo backend is only supported on desktop targets");

use self::monitor::MonitorExt;
use http::{Request, Response};
use raw_window_handle::{DisplayHandle, HasDisplayHandle, HasWindowHandle};
use std::borrow::Cow;

use tauri_runtime::{
  dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Position, Size},
  monitor::Monitor,
  webview::{DetachedWebview, DownloadEvent, PendingWebview, WebviewIpcHandler},
  window::{
    CursorIcon, DetachedWindow, DetachedWindowWebview, DragDropEvent, PendingWindow, RawWindow,
    WebviewEvent, WindowBuilder, WindowBuilderBase, WindowEvent, WindowId, WindowSizeConstraints,
  },
  Cookie, DeviceEventFilter, Error, EventLoopProxy, ExitRequestedEventAction, Icon,
  ProgressBarState, ProgressBarStatus, Result, RunEvent, Runtime, RuntimeHandle, RuntimeInitArgs,
  UserAttentionType, UserEvent, WebviewDispatch, WebviewEventId, WindowDispatch, WindowEventId,
};

pub use tao;
#[cfg(target_os = "android")]
use tao::platform::android::{WindowBuilderExtAndroid, WindowExtAndroid};
#[cfg(target_os = "macos")]
use tao::platform::macos::{EventLoopWindowTargetExtMacOS, WindowBuilderExtMacOS};
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
use tao::platform::unix::{WindowBuilderExtUnix, WindowExtUnix};
#[cfg(windows)]
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
pub use tao::window::{Window, WindowBuilder as TaoWindowBuilder, WindowId as TaoWindowId};
use tao::{
  event::{Event, StartCause, WindowEvent as TaoWindowEvent},
  event_loop::{
    ControlFlow, DeviceEventFilter as TaoDeviceEventFilter, EventLoop, EventLoopBuilder,
    EventLoopProxy as TaoEventLoopProxy, EventLoopWindowTarget,
  },
  monitor::MonitorHandle,
  window::{
    CursorIcon as TaoCursorIcon, Fullscreen, Icon as TaoWindowIcon,
    ProgressBarState as TaoProgressBarState, ProgressState as TaoProgressState, Theme as TaoTheme,
    UserAttentionType as TaoUserAttentionType,
  },
};
use tauri_utils::config::PreventOverflowConfig;
#[cfg(target_os = "macos")]
use tauri_utils::TitleBarStyle;
use tauri_utils::{
  config::{Color, WindowConfig},
  Theme,
};
use url::Url;
#[cfg(windows)]
use windows::Win32::Foundation::HWND;

pub use crate::servo::{WebViewBuilderExtServo, WebViewExtServo};

#[cfg(target_os = "ios")]
pub use tao::platform::ios::{WindowBuilderExtIOS, WindowExtIOS};
#[cfg(target_os = "macos")]
pub use tao::platform::macos::{
  ActivationPolicy as TaoActivationPolicy, EventLoopExtMacOS, WindowExtMacOS,
};
#[cfg(target_os = "macos")]
use tauri_runtime::ActivationPolicy;

use std::{
  cell::RefCell,
  collections::{BTreeMap, HashMap},
  fmt,
  ops::Deref,
  path::PathBuf,
  rc::Rc,
  sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc::{channel, Sender},
    Arc, Mutex, Weak,
  },
  thread::{current as current_thread, ThreadId},
};

pub type WebviewId = u32;
type IpcHandler = dyn Fn(Request<String>) + 'static;

mod monitor;
pub mod servo;
#[cfg(windows)]
mod undecorated_resizing;
mod util;
mod webview;
mod window;

pub use webview::Webview;
use window::WindowExt as _;

pub mod error;
pub use error::{Error as ServoError, Result as ServoResult};

pub use cookie;
pub use http;

/// JavaScript that replaces Tauri's default invoke system with one compatible
/// with Servo's IPC bridge.
///
/// Servo does not expose custom protocol request bodies to embedders, so the
/// default invoke system's custom protocol path cannot carry invoke payloads.
/// Pass this script to `tauri::Builder::invoke_system` when using this
/// runtime:
///
/// ```ignore
/// tauri::Builder::<tauri_runtime_servo::Servo<tauri::EventLoopMessage>>::new()
///   .invoke_system(tauri_runtime_servo::INVOKE_SYSTEM_SCRIPT)
///   .run(tauri::generate_context!())
///   .unwrap();
/// ```
pub const INVOKE_SYSTEM_SCRIPT: &str = include_str!("./invoke-system-initialization-script.js");
pub use url;

/// Type alias for a color in the RGBA format.
///
/// Each value can be 0..255 inclusive.
pub type RGBA = (u8, u8, u8, u8);

/// A rectangular region.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
  /// Rect position.
  pub position: dpi::Position,
  /// Rect size.
  pub size: dpi::Size,
}

impl Default for Rect {
  fn default() -> Self {
    Self {
      position: dpi::LogicalPosition::new(0, 0).into(),
      size: dpi::LogicalSize::new(0, 0).into(),
    }
  }
}

/// Type of page loading event.
pub enum PageLoadEvent {
  /// Indicates that the content of the page has started loading.
  Started,
  /// Indicates that the page content has finished loading.
  Finished,
}

/// An initialization script.
#[derive(Debug, Clone)]
pub struct InitializationScript {
  /// The script to run.
  pub script: String,
  /// Whether the script should be injected to main frame only.
  ///
  /// Servo injects scripts into every frame; this flag is kept for API parity.
  pub for_main_frame_only: bool,
}

/// Resolves a custom protocol [`Request`] asynchronously.
///
/// See [`WebViewBuilder::with_asynchronous_custom_protocol`] for more information.
pub struct RequestAsyncResponder {
  pub(crate) responder: AsyncResponderFn,
}

/// Boxed closure resolving an asynchronous custom protocol request.
pub(crate) type AsyncResponderFn = Box<dyn FnOnce(Response<Cow<'static, [u8]>>)>;

/// Boxed handler for a custom protocol request.
type CustomProtocolHandler =
  Box<dyn Fn(WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + Send + Sync>;

/// Boxed handler deciding whether a download may start (may rewrite the path).
type DownloadStartedHandler = Box<dyn FnMut(String, &mut PathBuf) -> bool>;

/// Boxed handler observing a finished (or failed) download.
type DownloadCompletedHandler = Box<dyn Fn(String, Option<PathBuf>, bool)>;

// SAFETY: even though the webview bindings do not indicate the responder is Send,
// it actually is and we need it in order to let the user do the protocol computation
// on a separate thread or async task.
unsafe impl Send for RequestAsyncResponder {}

impl RequestAsyncResponder {
  /// Resolves the request with the given response.
  pub fn respond<T: Into<Cow<'static, [u8]>>>(self, response: Response<T>) {
    let (parts, body) = response.into_parts();
    (self.responder)(Response::from_parts(parts, body.into()))
  }
}

/// An id that will be passed when this webview makes requests in certain callbacks.
pub type WebViewId<'a> = &'a str;

// WebViewAttributes is not stable enough to be pub.
struct WebViewAttributes<'a> {
  pub id: Option<WebViewId<'a>>,
  pub visible: bool,
  pub transparent: bool,
  pub background_color: Option<RGBA>,
  pub url: Option<String>,
  pub headers: Option<http::HeaderMap>,
  pub html: Option<String>,
  pub bounds: Option<Rect>,
  pub initialization_scripts: Vec<InitializationScript>,
  pub custom_protocols: HashMap<String, CustomProtocolHandler>,
  pub ipc_handler: Option<Box<dyn Fn(Request<String>)>>,
  pub navigation_handler: Option<Box<dyn Fn(String) -> bool>>,
  pub document_title_changed_handler: Option<Box<dyn Fn(String)>>,
  pub on_page_load_handler: Option<Box<dyn Fn(PageLoadEvent, String)>>,
  pub download_started_handler: Option<DownloadStartedHandler>,
  pub download_completed_handler: Option<DownloadCompletedHandler>,
  pub javascript_disabled: bool,
  pub user_agent: Option<String>,
  pub proxy_config: Option<ProxyConfig>,
}

impl Default for WebViewAttributes<'_> {
  fn default() -> Self {
    Self {
      id: None,
      visible: true,
      transparent: false,
      background_color: None,
      url: None,
      headers: None,
      html: None,
      bounds: None,
      initialization_scripts: Vec::new(),
      custom_protocols: HashMap::new(),
      ipc_handler: None,
      navigation_handler: None,
      document_title_changed_handler: None,
      on_page_load_handler: None,
      download_started_handler: None,
      download_completed_handler: None,
      javascript_disabled: false,
      user_agent: None,
      proxy_config: None,
    }
  }
}

/// The fundamental type to present a [`WebView`].
///
/// [`WebViewBuilder`] / [`WebView`] are the basic building blocks to construct WebView contents.
pub struct WebView {
  pub(crate) webview: servo::InnerWebView,
}

impl std::ops::Deref for WebView {
  type Target = servo::InnerWebView;

  #[inline(always)]
  fn deref(&self) -> &Self::Target {
    &self.webview
  }
}

impl WebView {
  /// Returns the id of this webview.
  pub fn id(&self) -> WebViewId<'_> {
    self.webview.id()
  }

  /// Returns a reference to the underlying Servo embedder.
  pub fn servo(&self) -> &servo::embedder::Embedder {
    self.webview.embedder()
  }
}

/// Webview builder that is used to create a [`WebView`].
pub struct WebViewBuilder<'a> {
  pub(crate) attrs: WebViewAttributes<'a>,
  /// Records errors before the [`WebViewBuilder::build_servo`] is called.
  pub(crate) error: crate::ServoResult<()>,
}

impl<'a> WebViewBuilder<'a> {
  /// Create a new [`WebViewBuilder`].
  pub fn new() -> Self {
    Self {
      attrs: WebViewAttributes::default(),
      error: Ok(()),
    }
  }

  /// Set an id that will be passed when this webview makes requests in certain callbacks.
  pub fn with_id(mut self, id: WebViewId<'a>) -> Self {
    self.attrs.id = Some(id);
    self
  }

  /// Sets whether the WebView should be visible.
  pub fn with_visible(mut self, visible: bool) -> Self {
    self.attrs.visible = visible;
    self
  }

  /// Sets whether the WebView should be transparent.
  pub fn with_transparent(mut self, transparent: bool) -> Self {
    self.attrs.transparent = transparent;
    self
  }

  /// Specify the webview background color. This will be ignored if `transparent` is set to `true`.
  pub fn with_background_color(mut self, background_color: RGBA) -> Self {
    self.attrs.background_color = Some(background_color);
    self
  }

  /// Disables JavaScript.
  pub fn with_javascript_disabled(mut self) -> Self {
    self.attrs.javascript_disabled = true;
    self
  }

  /// Add an initialization script to run on every page load.
  pub fn with_initialization_script<S: Into<String>>(self, js: S) -> Self {
    self.with_initialization_script_for_main_only(js, true)
  }

  /// Add an initialization script to run on every page load in the main frame.
  pub fn with_initialization_script_for_main_only<S: Into<String>>(
    mut self,
    js: S,
    for_main_frame_only: bool,
  ) -> Self {
    self
      .attrs
      .initialization_scripts
      .push(InitializationScript {
        script: js.into(),
        for_main_frame_only,
      });
    self
  }

  /// Register an asynchronous custom protocol.
  pub fn with_asynchronous_custom_protocol<F>(mut self, name: String, handler: F) -> Self
  where
    F: Fn(WebViewId, Request<Vec<u8>>, RequestAsyncResponder) + Send + Sync + 'static,
  {
    self.attrs.custom_protocols.insert(name, Box::new(handler));
    self
  }

  /// Set the ipc handler.
  pub fn with_ipc_handler<F>(mut self, handler: F) -> Self
  where
    F: Fn(Request<String>) + 'static,
  {
    self.attrs.ipc_handler = Some(Box::new(handler));
    self
  }

  /// Set the navigation handler.
  pub fn with_navigation_handler<F>(mut self, callback: F) -> Self
  where
    F: Fn(String) -> bool + 'static,
  {
    self.attrs.navigation_handler = Some(Box::new(callback));
    self
  }

  /// Set the document title changed handler.
  pub fn with_document_title_changed_handler<F>(mut self, callback: F) -> Self
  where
    F: Fn(String) + 'static,
  {
    self.attrs.document_title_changed_handler = Some(Box::new(callback));
    self
  }

  /// Set the page load handler.
  pub fn with_on_page_load_handler<F>(mut self, callback: F) -> Self
  where
    F: Fn(PageLoadEvent, String) + 'static,
  {
    self.attrs.on_page_load_handler = Some(Box::new(callback));
    self
  }

  /// Set the download started handler.
  pub fn with_download_started_handler<F>(mut self, callback: F) -> Self
  where
    F: FnMut(String, &mut PathBuf) -> bool + 'static,
  {
    self.attrs.download_started_handler = Some(Box::new(callback));
    self
  }

  /// Set the download completed handler.
  pub fn with_download_completed_handler<F>(mut self, callback: F) -> Self
  where
    F: Fn(String, Option<PathBuf>, bool) + 'static,
  {
    self.attrs.download_completed_handler = Some(Box::new(callback));
    self
  }

  /// Set the url to be loaded.
  pub fn with_url(mut self, url: impl Into<String>) -> Self {
    self.attrs.url = Some(url.into());
    self
  }

  /// Set the bounds of the webview.
  pub fn with_bounds(mut self, bounds: Rect) -> Self {
    self.attrs.bounds = Some(bounds);
    self
  }

  /// Set the user agent.
  pub fn with_user_agent(mut self, user_agent: &str) -> Self {
    self.attrs.user_agent = Some(user_agent.into());
    self
  }

  /// Set the proxy configuration.
  pub fn with_proxy_config(mut self, config: ProxyConfig) -> Self {
    self.attrs.proxy_config = Some(config);
    self
  }
}

impl Default for WebViewBuilder<'_> {
  fn default() -> Self {
    Self::new()
  }
}

/// The configuration of a proxy server.
#[derive(Debug, Clone)]
pub enum ProxyConfig {
  /// A HTTP proxy.
  Http(ProxyEndpoint),
  /// A SOCKS5 proxy.
  Socks5(ProxyEndpoint),
}

/// An endpoint (host and port) of a proxy server.
#[derive(Debug, Clone)]
pub struct ProxyEndpoint {
  /// The host of the proxy server.
  pub host: String,
  /// The port of the proxy server.
  pub port: u16,
}

// window
pub type WindowEventHandler = Box<dyn Fn(&WindowEvent) + Send>;
pub type WindowEventListeners = Arc<Mutex<HashMap<WindowEventId, WindowEventHandler>>>;
pub type WebviewEventHandler = Box<dyn Fn(&WebviewEvent) + Send>;
pub type WebviewEventListeners = Arc<Mutex<HashMap<WebviewEventId, WebviewEventHandler>>>;

#[derive(Debug, Clone, Default)]
pub struct WindowIdStore(Arc<Mutex<HashMap<TaoWindowId, WindowId>>>);

impl WindowIdStore {
  pub fn insert(&self, w: TaoWindowId, id: WindowId) {
    self.0.lock().unwrap().insert(w, id);
  }

  pub fn get(&self, w: &TaoWindowId) -> Option<WindowId> {
    self.0.lock().unwrap().get(w).copied()
  }
}

#[macro_export]
macro_rules! getter {
  ($self: ident, $rx: expr, $message: expr) => {{
    $crate::send_user_message(&$self.context, $message)?;
    $rx
      .recv()
      .map_err(|_| $crate::Error::FailedToReceiveMessage)
  }};
}

macro_rules! window_getter {
  ($self: ident, $message: expr) => {{
    let (tx, rx) = channel();
    getter!($self, rx, Message::Window($self.window_id, $message(tx)))
  }};
}

macro_rules! event_loop_window_getter {
  ($self: ident, $message: expr) => {{
    let (tx, rx) = channel();
    getter!($self, rx, Message::EventLoopWindowTarget($message(tx)))
  }};
}

macro_rules! webview_getter {
  ($self: ident, $message: expr) => {{
    let (tx, rx) = channel();
    getter!(
      $self,
      rx,
      Message::Webview(
        *$self.window_id.lock().unwrap(),
        $self.webview_id,
        $message(tx)
      )
    )
  }};
}

pub(crate) fn send_user_message<T: UserEvent>(
  context: &Context<T>,
  message: Message<T>,
) -> Result<()> {
  if current_thread().id() == context.main_thread_id {
    handle_user_message(
      &context.main_thread.window_target,
      message,
      UserMessageContext {
        window_id_map: &context.window_id_map,
        windows: &context.main_thread.windows,
      },
    );
    Ok(())
  } else {
    context
      .proxy
      .send_event(message)
      .map_err(|_| Error::FailedToSendMessage)
  }
}

#[derive(Clone)]
pub struct Context<T: UserEvent> {
  pub window_id_map: WindowIdStore,
  main_thread_id: ThreadId,
  pub proxy: TaoEventLoopProxy<Message<T>>,
  main_thread: DispatcherMainThreadContext<T>,
  next_window_id: Arc<AtomicU32>,
  next_webview_id: Arc<AtomicU32>,
  next_window_event_id: Arc<AtomicU32>,
  next_webview_event_id: Arc<AtomicU32>,
}

unsafe impl<T: UserEvent> Send for Context<T> {}
unsafe impl<T: UserEvent> Sync for Context<T> {}

impl<T: UserEvent> Context<T> {
  pub fn run_threaded<R, F>(&self, f: F) -> R
  where
    F: FnOnce(Option<&DispatcherMainThreadContext<T>>) -> R,
  {
    f(if current_thread().id() == self.main_thread_id {
      Some(&self.main_thread)
    } else {
      None
    })
  }

  fn next_window_id(&self) -> WindowId {
    self.next_window_id.fetch_add(1, Ordering::Relaxed).into()
  }

  fn next_webview_id(&self) -> WebviewId {
    self.next_webview_id.fetch_add(1, Ordering::Relaxed)
  }

  fn next_window_event_id(&self) -> u32 {
    self.next_window_event_id.fetch_add(1, Ordering::Relaxed)
  }

  fn next_webview_event_id(&self) -> u32 {
    self.next_webview_event_id.fetch_add(1, Ordering::Relaxed)
  }
}

impl<T: UserEvent> Context<T> {
  fn create_window<F: Fn(RawWindow) + Send + 'static>(
    &self,
    pending: PendingWindow<T, Servo<T>>,
    after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Servo<T>>> {
    let label = pending.label.clone();
    let context = self.clone();
    let window_id = self.next_window_id();
    let (webview_id, use_https_scheme) = pending
      .webview
      .as_ref()
      .map(|w| {
        (
          Some(context.next_webview_id()),
          w.webview_attributes.use_https_scheme,
        )
      })
      .unwrap_or((None, false));

    let (tx, rx) = channel();
    send_user_message(
      self,
      Message::CreateWindow(
        window_id,
        Box::new(move |event_loop| {
          create_window(
            window_id,
            webview_id.unwrap_or_default(),
            event_loop,
            &context,
            pending,
            after_window_creation,
          )
        }),
        tx,
      ),
    )?;
    rx.recv()
      .map_err(|_| crate::Error::FailedToReceiveMessage)??;

    let dispatcher = ServoWindowDispatcher {
      window_id,
      context: self.clone(),
    };

    let detached_webview = webview_id.map(|id| {
      let webview = DetachedWebview {
        label: label.clone(),
        dispatcher: ServoWebviewDispatcher {
          window_id: Arc::new(Mutex::new(window_id)),
          webview_id: id,
          context: self.clone(),
        },
      };
      DetachedWindowWebview {
        webview,
        use_https_scheme,
      }
    });

    Ok(DetachedWindow {
      id: window_id,
      label,
      dispatcher,
      webview: detached_webview,
    })
  }

  fn create_webview(
    &self,
    window_id: WindowId,
    pending: PendingWebview<T, Servo<T>>,
  ) -> Result<DetachedWebview<T, Servo<T>>> {
    let label = pending.label.clone();
    let context = self.clone();

    let webview_id = self.next_webview_id();

    let window_id_wrapper = Arc::new(Mutex::new(window_id));
    let window_id_wrapper_ = window_id_wrapper.clone();

    let (tx, rx) = channel();
    send_user_message(
      self,
      Message::CreateWebview(
        window_id,
        Box::new(move |window, options| {
          create_webview(
            WebviewKind::WindowChild,
            window,
            window_id_wrapper_,
            webview_id,
            &context,
            pending,
            options.focused_webview,
          )
        }),
        tx,
      ),
    )?;
    rx.recv()
      .map_err(|_| crate::Error::FailedToReceiveMessage)??;

    let dispatcher = ServoWebviewDispatcher {
      window_id: window_id_wrapper,
      webview_id,
      context: self.clone(),
    };

    Ok(DetachedWebview { label, dispatcher })
  }
}

#[cfg(feature = "tracing")]
#[derive(Debug, Clone, Default)]
pub struct ActiveTraceSpanStore(Rc<RefCell<Vec<ActiveTracingSpan>>>);

#[cfg(feature = "tracing")]
impl ActiveTraceSpanStore {
  pub fn remove_window_draw(&self) {
    self
      .0
      .borrow_mut()
      .retain(|t| !matches!(t, ActiveTracingSpan::WindowDraw { id: _, span: _ }));
  }
}

#[cfg(feature = "tracing")]
#[derive(Debug)]
pub enum ActiveTracingSpan {
  WindowDraw {
    id: TaoWindowId,
    span: tracing::span::EnteredSpan,
  },
}

#[derive(Debug)]
pub struct WindowsStore(pub RefCell<BTreeMap<WindowId, WindowWrapper>>);

#[derive(Debug, Clone)]
pub struct DispatcherMainThreadContext<T: UserEvent> {
  pub window_target: EventLoopWindowTarget<Message<T>>,
  // changing this to an Rc will cause frequent app crashes.
  pub windows: Arc<WindowsStore>,
  #[cfg(feature = "tracing")]
  pub active_tracing_spans: ActiveTraceSpanStore,
}

impl<T: UserEvent> fmt::Debug for Context<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Context")
      .field("main_thread_id", &self.main_thread_id)
      .field("proxy", &self.proxy)
      .field("main_thread", &self.main_thread)
      .finish()
  }
}

pub struct DeviceEventFilterWrapper(pub TaoDeviceEventFilter);

impl From<DeviceEventFilter> for DeviceEventFilterWrapper {
  fn from(item: DeviceEventFilter) -> Self {
    match item {
      DeviceEventFilter::Always => Self(TaoDeviceEventFilter::Always),
      DeviceEventFilter::Never => Self(TaoDeviceEventFilter::Never),
      DeviceEventFilter::Unfocused => Self(TaoDeviceEventFilter::Unfocused),
    }
  }
}

pub struct RectWrapper(pub Rect);
impl From<tauri_runtime::dpi::Rect> for RectWrapper {
  fn from(value: tauri_runtime::dpi::Rect) -> Self {
    RectWrapper(Rect {
      position: value.position,
      size: value.size,
    })
  }
}

/// Wrapper around a [`tao::window::Icon`] that can be created from an [`Icon`].
pub struct TaoIcon(pub TaoWindowIcon);

impl TryFrom<Icon<'_>> for TaoIcon {
  type Error = Error;
  fn try_from(icon: Icon<'_>) -> std::result::Result<Self, Self::Error> {
    TaoWindowIcon::from_rgba(icon.rgba.to_vec(), icon.width, icon.height)
      .map(Self)
      .map_err(|e| Error::InvalidIcon(Box::new(e)))
  }
}

pub struct WindowEventWrapper(pub Option<WindowEvent>);

impl WindowEventWrapper {
  fn map_from_tao(event: &TaoWindowEvent<'_>, #[cfg(windows)] window: &WindowWrapper) -> Self {
    let event = match event {
      TaoWindowEvent::Resized(size) => WindowEvent::Resized(*size),
      TaoWindowEvent::Moved(position) => WindowEvent::Moved(*position),
      TaoWindowEvent::Destroyed => WindowEvent::Destroyed,
      TaoWindowEvent::ScaleFactorChanged {
        scale_factor,
        new_inner_size,
      } => WindowEvent::ScaleFactorChanged {
        scale_factor: *scale_factor,
        new_inner_size: **new_inner_size,
      },
      TaoWindowEvent::Focused(focused) => {
        #[cfg(not(windows))]
        return Self(Some(WindowEvent::Focused(*focused)));
        // on multiwebview mode, if there's no focused webview, it means we're receiving a direct window focus change
        // (without receiving a webview focus, such as when clicking the taskbar app icon or using Alt + Tab)
        // in this case we must send the focus change event here
        #[cfg(windows)]
        if window.has_children.load(Ordering::Relaxed) {
          if !*focused {
            // Blur events are handled in the webview side (add_LostFocus)
            return Self(None);
          }

          let mut focused_webview = window.focused_webview.lock().unwrap();
          if let FocusState::Blured {
            last_focused_webview_label,
          } = &*focused_webview
          {
            let should_focus_webview =
              last_focused_webview_label
                .as_deref()
                .and_then(|last_focused_webview_label| {
                  window
                    .webviews
                    .iter()
                    .find(|w| w.label == last_focused_webview_label)
                });
            *focused_webview = FocusState::WindowFocused;
            if let Some(should_focus_webview) = should_focus_webview {
              drop(focused_webview);
              let _ = should_focus_webview.focus();
            }
            WindowEvent::Focused(true)
          } else {
            // Already focused
            return Self(None);
          }
        } else if window.webviews.is_empty() {
          // Raw tao window without webviews, forward the event
          WindowEvent::Focused(*focused)
        } else {
          // when not on multiwebview mode, focus changes are handled on the webview
          return Self(None);
        }
      }
      TaoWindowEvent::ThemeChanged(theme) => WindowEvent::ThemeChanged(map_theme(theme)),
      #[cfg(mobile)]
      TaoWindowEvent::Suspended => WindowEvent::Suspended,
      #[cfg(mobile)]
      TaoWindowEvent::Resumed => WindowEvent::Resumed,
      _ => return Self(None),
    };
    Self(Some(event))
  }

  fn parse(window: &WindowWrapper, event: &TaoWindowEvent<'_>) -> Self {
    match event {
      // resized event from tao doesn't include a reliable size on macOS
      // because the webview replaces the NSView
      TaoWindowEvent::Resized(_) => {
        if let Some(w) = &window.inner {
          let size = inner_size(
            w,
            &window.webviews,
            window.has_children.load(Ordering::Relaxed),
          );
          Self(Some(WindowEvent::Resized(size)))
        } else {
          Self(None)
        }
      }
      e => Self::map_from_tao(
        e,
        #[cfg(windows)]
        window,
      ),
    }
  }
}

pub fn map_theme(theme: &TaoTheme) -> Theme {
  match theme {
    TaoTheme::Light => Theme::Light,
    TaoTheme::Dark => Theme::Dark,
    _ => Theme::Light,
  }
}

#[cfg(target_os = "macos")]
fn tao_activation_policy(activation_policy: ActivationPolicy) -> TaoActivationPolicy {
  match activation_policy {
    ActivationPolicy::Regular => TaoActivationPolicy::Regular,
    ActivationPolicy::Accessory => TaoActivationPolicy::Accessory,
    ActivationPolicy::Prohibited => TaoActivationPolicy::Prohibited,
    _ => unimplemented!(),
  }
}

pub struct MonitorHandleWrapper(pub MonitorHandle);

impl From<MonitorHandleWrapper> for Monitor {
  fn from(monitor: MonitorHandleWrapper) -> Monitor {
    Self {
      name: monitor.0.name(),
      position: monitor.0.position(),
      size: monitor.0.size(),
      work_area: monitor.0.work_area(),
      scale_factor: monitor.0.scale_factor(),
    }
  }
}

#[cfg(desktop)]
fn find_monitor_for_position(
  monitors: impl Iterator<Item = MonitorHandle>,
  window_position: Position,
) -> Option<MonitorHandle> {
  monitors.into_iter().find(|m| {
    let monitor_pos = m.position();
    let monitor_size = m.size();

    // type annotations required for 32bit targets.
    let window_position = window_position.to_physical::<i32>(m.scale_factor());

    monitor_pos.x <= window_position.x
      && window_position.x < monitor_pos.x + monitor_size.width as i32
      && monitor_pos.y <= window_position.y
      && window_position.y < monitor_pos.y + monitor_size.height as i32
  })
}

#[derive(Debug, Clone)]
pub struct UserAttentionTypeWrapper(pub TaoUserAttentionType);

impl From<UserAttentionType> for UserAttentionTypeWrapper {
  fn from(request_type: UserAttentionType) -> Self {
    let o = match request_type {
      UserAttentionType::Critical => TaoUserAttentionType::Critical,
      UserAttentionType::Informational => TaoUserAttentionType::Informational,
    };
    Self(o)
  }
}

#[derive(Debug)]
pub struct CursorIconWrapper(pub TaoCursorIcon);

impl From<CursorIcon> for CursorIconWrapper {
  fn from(icon: CursorIcon) -> Self {
    use CursorIcon::*;
    let i = match icon {
      Default => TaoCursorIcon::Default,
      Crosshair => TaoCursorIcon::Crosshair,
      Hand => TaoCursorIcon::Hand,
      Arrow => TaoCursorIcon::Arrow,
      Move => TaoCursorIcon::Move,
      Text => TaoCursorIcon::Text,
      Wait => TaoCursorIcon::Wait,
      Help => TaoCursorIcon::Help,
      Progress => TaoCursorIcon::Progress,
      NotAllowed => TaoCursorIcon::NotAllowed,
      ContextMenu => TaoCursorIcon::ContextMenu,
      Cell => TaoCursorIcon::Cell,
      VerticalText => TaoCursorIcon::VerticalText,
      Alias => TaoCursorIcon::Alias,
      Copy => TaoCursorIcon::Copy,
      NoDrop => TaoCursorIcon::NoDrop,
      Grab => TaoCursorIcon::Grab,
      Grabbing => TaoCursorIcon::Grabbing,
      AllScroll => TaoCursorIcon::AllScroll,
      ZoomIn => TaoCursorIcon::ZoomIn,
      ZoomOut => TaoCursorIcon::ZoomOut,
      EResize => TaoCursorIcon::EResize,
      NResize => TaoCursorIcon::NResize,
      NeResize => TaoCursorIcon::NeResize,
      NwResize => TaoCursorIcon::NwResize,
      SResize => TaoCursorIcon::SResize,
      SeResize => TaoCursorIcon::SeResize,
      SwResize => TaoCursorIcon::SwResize,
      WResize => TaoCursorIcon::WResize,
      EwResize => TaoCursorIcon::EwResize,
      NsResize => TaoCursorIcon::NsResize,
      NeswResize => TaoCursorIcon::NeswResize,
      NwseResize => TaoCursorIcon::NwseResize,
      ColResize => TaoCursorIcon::ColResize,
      RowResize => TaoCursorIcon::RowResize,
      _ => TaoCursorIcon::Default,
    };
    Self(i)
  }
}

pub struct ProgressStateWrapper(pub TaoProgressState);

impl From<ProgressBarStatus> for ProgressStateWrapper {
  fn from(status: ProgressBarStatus) -> Self {
    let state = match status {
      ProgressBarStatus::None => TaoProgressState::None,
      ProgressBarStatus::Normal => TaoProgressState::Normal,
      ProgressBarStatus::Indeterminate => TaoProgressState::Indeterminate,
      ProgressBarStatus::Paused => TaoProgressState::Paused,
      ProgressBarStatus::Error => TaoProgressState::Error,
    };
    Self(state)
  }
}

pub struct ProgressBarStateWrapper(pub TaoProgressBarState);

impl From<ProgressBarState> for ProgressBarStateWrapper {
  fn from(progress_state: ProgressBarState) -> Self {
    Self(TaoProgressBarState {
      progress: progress_state.progress,
      state: progress_state
        .status
        .map(|state| ProgressStateWrapper::from(state).0),
      desktop_filename: progress_state.desktop_filename,
    })
  }
}

#[derive(Clone, Default)]
pub struct WindowBuilderWrapper {
  inner: TaoWindowBuilder,
  center: bool,
  prevent_overflow: Option<Size>,
  #[cfg(target_os = "macos")]
  tabbing_identifier: Option<String>,
}

impl std::fmt::Debug for WindowBuilderWrapper {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut s = f.debug_struct("WindowBuilderWrapper");
    s.field("inner", &self.inner)
      .field("center", &self.center)
      .field("prevent_overflow", &self.prevent_overflow);
    #[cfg(target_os = "macos")]
    {
      s.field("tabbing_identifier", &self.tabbing_identifier);
    }
    s.finish()
  }
}

// SAFETY: this type is `Send` since `menu_items` are read only here
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for WindowBuilderWrapper {}

impl WindowBuilderBase for WindowBuilderWrapper {}
impl WindowBuilder for WindowBuilderWrapper {
  fn new() -> Self {
    #[allow(unused_mut)]
    let mut builder = Self::default().focused(true);

    #[cfg(target_os = "macos")]
    {
      // TODO: find a proper way to prevent webview being pushed out of the window.
      // Workaround for issue: https://github.com/tauri-apps/tauri/issues/10225
      // The window requires `NSFullSizeContentViewWindowMask` flag to prevent devtools
      // pushing the content view out of the window.
      // By setting the default style to `TitleBarStyle::Visible` should fix the issue for most of the users.
      builder = builder.title_bar_style(TitleBarStyle::Visible);
    }

    builder = builder.title("Tauri App");

    #[cfg(windows)]
    {
      builder = builder.window_classname("Tauri Window");
    }

    builder
  }

  fn with_config(config: &WindowConfig) -> Self {
    let mut window = WindowBuilderWrapper::new();

    #[cfg(target_os = "macos")]
    {
      window = window
        .hidden_title(config.hidden_title)
        .title_bar_style(config.title_bar_style);
      if let Some(identifier) = &config.tabbing_identifier {
        window = window.tabbing_identifier(identifier);
      }
      if let Some(position) = &config.traffic_light_position {
        window = window.traffic_light_position(tauri_runtime::dpi::LogicalPosition::new(
          position.x, position.y,
        ));
      }
    }

    #[cfg(any(not(target_os = "macos"), feature = "macos-private-api"))]
    {
      window = window.transparent(config.transparent);
    }
    #[cfg(all(
      target_os = "macos",
      not(feature = "macos-private-api"),
      debug_assertions
    ))]
    if config.transparent {
      eprintln!(
        "The window is set to be transparent but the `macos-private-api` is not enabled.
        This can be enabled via the `tauri.macOSPrivateApi` configuration property <https://v2.tauri.app/reference/config/#macosprivateapi>
      ");
    }

    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    {
      // Mouse event is disabled on Linux since sudden event bursts could block event loop.
      window.inner = window.inner.with_cursor_moved_event(false);
    }

    #[cfg(target_os = "android")]
    {
      if let Some(activity_name) = &config.activity_name {
        window.inner = window.inner.with_activity_name(activity_name.clone());
      }
      if let Some(activity_name) = &config.created_by_activity_name {
        window.inner = window
          .inner
          .with_created_by_activity_name(activity_name.clone());
      }
    }

    #[cfg(target_os = "ios")]
    {
      if let Some(scene_identifier) = &config.requested_by_scene_identifier {
        window.inner = window
          .inner
          .with_requesting_scene_identifier(scene_identifier.clone());
      }
    }

    // ignore size from config for mobile for backward compatibility
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
      window = window.inner_size(config.width, config.height);
    }

    #[cfg(any(not(target_os = "macos"), feature = "macos-private-api"))]
    {
      window = window.transparent(config.transparent);
    }

    window = window
      .title(config.title.to_string())
      .focused(config.focus)
      .focusable(config.focusable)
      .visible(config.visible)
      .resizable(config.resizable)
      .fullscreen(config.fullscreen)
      .decorations(config.decorations)
      .maximized(config.maximized)
      .always_on_bottom(config.always_on_bottom)
      .always_on_top(config.always_on_top)
      .visible_on_all_workspaces(config.visible_on_all_workspaces)
      .content_protected(config.content_protected)
      .skip_taskbar(config.skip_taskbar)
      .theme(config.theme)
      .closable(config.closable)
      .maximizable(config.maximizable)
      .minimizable(config.minimizable)
      .shadow(config.shadow);

    let mut constraints = WindowSizeConstraints::default();

    if let Some(min_width) = config.min_width {
      constraints.min_width = Some(tao::dpi::LogicalUnit::new(min_width).into());
    }
    if let Some(min_height) = config.min_height {
      constraints.min_height = Some(tao::dpi::LogicalUnit::new(min_height).into());
    }
    if let Some(max_width) = config.max_width {
      constraints.max_width = Some(tao::dpi::LogicalUnit::new(max_width).into());
    }
    if let Some(max_height) = config.max_height {
      constraints.max_height = Some(tao::dpi::LogicalUnit::new(max_height).into());
    }
    if let Some(color) = config.background_color {
      window = window.background_color(color);
    }
    window = window.inner_size_constraints(constraints);

    if let (Some(x), Some(y)) = (config.x, config.y) {
      window = window.position(x, y);
    }

    if config.center {
      window = window.center();
    }

    if let Some(window_classname) = &config.window_classname {
      window = window.window_classname(window_classname);
    }

    if let Some(prevent_overflow) = &config.prevent_overflow {
      window = match prevent_overflow {
        PreventOverflowConfig::Enable(true) => window.prevent_overflow(),
        PreventOverflowConfig::Margin(margin) => {
          window.prevent_overflow_with_margin(PhysicalSize::new(margin.width, margin.height).into())
        }
        _ => window,
      };
    }

    window
  }

  fn center(mut self) -> Self {
    self.center = true;
    self
  }

  fn position(mut self, x: f64, y: f64) -> Self {
    self.inner = self.inner.with_position(LogicalPosition::new(x, y));
    self
  }

  fn inner_size(mut self, width: f64, height: f64) -> Self {
    self.inner = self.inner.with_inner_size(LogicalSize::new(width, height));
    self
  }

  fn min_inner_size(mut self, min_width: f64, min_height: f64) -> Self {
    self.inner = self
      .inner
      .with_min_inner_size(LogicalSize::new(min_width, min_height));
    self
  }

  fn max_inner_size(mut self, max_width: f64, max_height: f64) -> Self {
    self.inner = self
      .inner
      .with_max_inner_size(LogicalSize::new(max_width, max_height));
    self
  }

  fn inner_size_constraints(mut self, constraints: WindowSizeConstraints) -> Self {
    self.inner.window.inner_size_constraints = tao::window::WindowSizeConstraints {
      min_width: constraints.min_width,
      min_height: constraints.min_height,
      max_width: constraints.max_width,
      max_height: constraints.max_height,
    };
    self
  }

  /// Prevent the window from overflowing the working area (e.g. monitor size - taskbar size) on creation
  ///
  /// ## Platform-specific
  ///
  /// - **iOS / Android:** Unsupported.
  fn prevent_overflow(mut self) -> Self {
    self
      .prevent_overflow
      .replace(PhysicalSize::new(0, 0).into());
    self
  }

  /// Prevent the window from overflowing the working area (e.g. monitor size - taskbar size)
  /// on creation with a margin
  ///
  /// ## Platform-specific
  ///
  /// - **iOS / Android:** Unsupported.
  fn prevent_overflow_with_margin(mut self, margin: Size) -> Self {
    self.prevent_overflow.replace(margin);
    self
  }

  fn resizable(mut self, resizable: bool) -> Self {
    self.inner = self.inner.with_resizable(resizable);
    self
  }

  fn maximizable(mut self, maximizable: bool) -> Self {
    self.inner = self.inner.with_maximizable(maximizable);
    self
  }

  fn minimizable(mut self, minimizable: bool) -> Self {
    self.inner = self.inner.with_minimizable(minimizable);
    self
  }

  fn closable(mut self, closable: bool) -> Self {
    self.inner = self.inner.with_closable(closable);
    self
  }

  fn title<S: Into<String>>(mut self, title: S) -> Self {
    self.inner = self.inner.with_title(title.into());
    self
  }

  fn fullscreen(mut self, fullscreen: bool) -> Self {
    self.inner = if fullscreen {
      self
        .inner
        .with_fullscreen(Some(Fullscreen::Borderless(None)))
    } else {
      self.inner.with_fullscreen(None)
    };
    self
  }

  fn focused(mut self, focused: bool) -> Self {
    self.inner = self.inner.with_focused(focused);
    self
  }

  fn focusable(mut self, focusable: bool) -> Self {
    self.inner = self.inner.with_focusable(focusable);
    self
  }

  fn maximized(mut self, maximized: bool) -> Self {
    self.inner = self.inner.with_maximized(maximized);
    self
  }

  fn visible(mut self, visible: bool) -> Self {
    self.inner = self.inner.with_visible(visible);
    self
  }

  #[cfg(any(not(target_os = "macos"), feature = "macos-private-api"))]
  fn transparent(mut self, transparent: bool) -> Self {
    self.inner = self.inner.with_transparent(transparent);
    self
  }

  fn decorations(mut self, decorations: bool) -> Self {
    self.inner = self.inner.with_decorations(decorations);
    self
  }

  fn always_on_bottom(mut self, always_on_bottom: bool) -> Self {
    self.inner = self.inner.with_always_on_bottom(always_on_bottom);
    self
  }

  fn always_on_top(mut self, always_on_top: bool) -> Self {
    self.inner = self.inner.with_always_on_top(always_on_top);
    self
  }

  fn visible_on_all_workspaces(mut self, visible_on_all_workspaces: bool) -> Self {
    self.inner = self
      .inner
      .with_visible_on_all_workspaces(visible_on_all_workspaces);
    self
  }

  fn content_protected(mut self, protected: bool) -> Self {
    self.inner = self.inner.with_content_protection(protected);
    self
  }

  fn shadow(#[allow(unused_mut)] mut self, _enable: bool) -> Self {
    #[cfg(windows)]
    {
      self.inner = self.inner.with_undecorated_shadow(_enable);
    }
    #[cfg(target_os = "macos")]
    {
      self.inner = self.inner.with_has_shadow(_enable);
    }
    self
  }

  #[cfg(windows)]
  fn owner(mut self, owner: HWND) -> Self {
    self.inner = self.inner.with_owner_window(owner.0 as _);
    self
  }

  #[cfg(windows)]
  fn parent(mut self, parent: HWND) -> Self {
    self.inner = self.inner.with_parent_window(parent.0 as _);
    self
  }

  #[cfg(target_os = "macos")]
  fn parent(mut self, parent: *mut std::ffi::c_void) -> Self {
    self.inner = self.inner.with_parent_window(parent);
    self
  }

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn transient_for(mut self, parent: &impl gtk::glib::IsA<gtk::Window>) -> Self {
    self.inner = self.inner.with_transient_for(parent);
    self
  }

  #[cfg(windows)]
  fn drag_and_drop(mut self, enabled: bool) -> Self {
    self.inner = self.inner.with_drag_and_drop(enabled);
    self
  }

  #[cfg(target_os = "macos")]
  fn title_bar_style(mut self, style: TitleBarStyle) -> Self {
    match style {
      TitleBarStyle::Visible => {
        self.inner = self.inner.with_titlebar_transparent(false);
        // Fixes rendering issue when resizing window with devtools open (https://github.com/tauri-apps/tauri/issues/3914)
        self.inner = self.inner.with_fullsize_content_view(true);
      }
      TitleBarStyle::Transparent => {
        self.inner = self.inner.with_titlebar_transparent(true);
        self.inner = self.inner.with_fullsize_content_view(false);
      }
      TitleBarStyle::Overlay => {
        self.inner = self.inner.with_titlebar_transparent(true);
        self.inner = self.inner.with_fullsize_content_view(true);
      }
      unknown => {
        #[cfg(feature = "tracing")]
        tracing::warn!("unknown title bar style applied: {unknown}");

        #[cfg(not(feature = "tracing"))]
        eprintln!("unknown title bar style applied: {unknown}");
      }
    }
    self
  }

  #[cfg(target_os = "macos")]
  fn traffic_light_position<P: Into<Position>>(mut self, position: P) -> Self {
    self.inner = self.inner.with_traffic_light_inset(position.into());
    self
  }

  #[cfg(target_os = "macos")]
  fn hidden_title(mut self, hidden: bool) -> Self {
    self.inner = self.inner.with_title_hidden(hidden);
    self
  }

  #[cfg(target_os = "macos")]
  fn tabbing_identifier(mut self, identifier: &str) -> Self {
    self.inner = self.inner.with_tabbing_identifier(identifier);
    self.tabbing_identifier.replace(identifier.into());
    self
  }

  fn icon(mut self, icon: Icon) -> Result<Self> {
    self.inner = self
      .inner
      .with_window_icon(Some(TaoIcon::try_from(icon)?.0));
    Ok(self)
  }

  fn background_color(mut self, color: Color) -> Self {
    self.inner = self.inner.with_background_color(color.into());
    self
  }

  #[cfg(any(
    windows,
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn skip_taskbar(mut self, skip: bool) -> Self {
    self.inner = self.inner.with_skip_taskbar(skip);
    self
  }

  #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
  fn skip_taskbar(self, _skip: bool) -> Self {
    self
  }

  fn theme(mut self, theme: Option<Theme>) -> Self {
    self.inner = self.inner.with_theme(if let Some(t) = theme {
      match t {
        Theme::Dark => Some(TaoTheme::Dark),
        _ => Some(TaoTheme::Light),
      }
    } else {
      None
    });

    self
  }

  fn has_icon(&self) -> bool {
    self.inner.window.window_icon.is_some()
  }

  fn get_theme(&self) -> Option<Theme> {
    self.inner.window.preferred_theme.map(|theme| match theme {
      TaoTheme::Dark => Theme::Dark,
      _ => Theme::Light,
    })
  }

  #[cfg(windows)]
  fn window_classname<S: Into<String>>(mut self, window_classname: S) -> Self {
    self.inner = self.inner.with_window_classname(window_classname);
    self
  }
  #[cfg(not(windows))]
  fn window_classname<S: Into<String>>(self, _window_classname: S) -> Self {
    self
  }

  #[cfg(target_os = "android")]
  fn activity_name<S: Into<String>>(mut self, class_name: S) -> Self {
    self.inner = self.inner.with_activity_name(class_name.into());
    self
  }

  #[cfg(target_os = "android")]
  fn created_by_activity_name<S: Into<String>>(mut self, class_name: S) -> Self {
    self.inner = self.inner.with_created_by_activity_name(class_name.into());
    self
  }

  #[cfg(target_os = "ios")]
  fn requested_by_scene_identifier<S: Into<String>>(mut self, identifier: S) -> Self {
    self.inner = self
      .inner
      .with_requesting_scene_identifier(identifier.into());
    self
  }
}

#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
pub struct GtkWindow(pub gtk::ApplicationWindow);
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for GtkWindow {}

#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
pub struct GtkBox(pub gtk::Box);
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for GtkBox {}

pub struct SendRawWindowHandle(pub raw_window_handle::RawWindowHandle);
unsafe impl Send for SendRawWindowHandle {}

pub enum ApplicationMessage {
  #[cfg(target_os = "macos")]
  Show,
  #[cfg(target_os = "macos")]
  Hide,
  #[cfg(any(target_os = "macos", target_os = "ios"))]
  FetchDataStoreIdentifiers(Box<dyn FnOnce(Vec<[u8; 16]>) + Send + 'static>),
  #[cfg(any(target_os = "macos", target_os = "ios"))]
  RemoveDataStore([u8; 16], Box<dyn FnOnce(Result<()>) + Send + 'static>),
}

pub enum WindowMessage {
  AddEventListener(WindowEventId, Box<dyn Fn(&WindowEvent) + Send>),
  // Getters
  ScaleFactor(Sender<f64>),
  InnerPosition(Sender<Result<PhysicalPosition<i32>>>),
  OuterPosition(Sender<Result<PhysicalPosition<i32>>>),
  InnerSize(Sender<PhysicalSize<u32>>),
  OuterSize(Sender<PhysicalSize<u32>>),
  IsFullscreen(Sender<bool>),
  IsMinimized(Sender<bool>),
  IsMaximized(Sender<bool>),
  IsFocused(Sender<bool>),
  IsDecorated(Sender<bool>),
  IsResizable(Sender<bool>),
  IsMaximizable(Sender<bool>),
  IsMinimizable(Sender<bool>),
  IsClosable(Sender<bool>),
  IsVisible(Sender<bool>),
  Title(Sender<String>),
  CurrentMonitor(Sender<Option<MonitorHandle>>),
  PrimaryMonitor(Sender<Option<MonitorHandle>>),
  MonitorFromPoint(Sender<Option<MonitorHandle>>, (f64, f64)),
  AvailableMonitors(Sender<Vec<MonitorHandle>>),
  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  GtkWindow(Sender<GtkWindow>),
  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  GtkBox(Sender<GtkBox>),
  #[cfg(target_os = "android")]
  ActivityName(Sender<String>),
  #[cfg(target_os = "ios")]
  SceneIdentifier(Sender<String>),
  RawWindowHandle(Sender<std::result::Result<SendRawWindowHandle, raw_window_handle::HandleError>>),
  Theme(Sender<Theme>),
  IsEnabled(Sender<bool>),
  IsAlwaysOnTop(Sender<bool>),
  // Setters
  Center,
  RequestUserAttention(Option<UserAttentionTypeWrapper>),
  SetEnabled(bool),
  SetResizable(bool),
  SetMaximizable(bool),
  SetMinimizable(bool),
  SetClosable(bool),
  SetTitle(String),
  Maximize,
  Unmaximize,
  Minimize,
  Unminimize,
  Show,
  Hide,
  Close,
  Destroy,
  SetDecorations(bool),
  SetShadow(bool),
  SetAlwaysOnBottom(bool),
  SetAlwaysOnTop(bool),
  SetVisibleOnAllWorkspaces(bool),
  SetContentProtected(bool),
  SetSize(Size),
  SetMinSize(Option<Size>),
  SetMaxSize(Option<Size>),
  SetSizeConstraints(WindowSizeConstraints),
  SetPosition(Position),
  SetFullscreen(bool),
  #[cfg(target_os = "macos")]
  SetSimpleFullscreen(bool),
  SetFocus,
  SetFocusable(bool),
  SetIcon(TaoWindowIcon),
  SetSkipTaskbar(bool),
  SetCursorGrab(bool),
  SetCursorVisible(bool),
  SetCursorIcon(CursorIcon),
  SetCursorPosition(Position),
  SetIgnoreCursorEvents(bool),
  SetBadgeCount(Option<i64>, Option<String>),
  SetBadgeLabel(Option<String>),
  SetOverlayIcon(Option<TaoIcon>),
  SetProgressBar(ProgressBarState),
  SetTitleBarStyle(tauri_utils::TitleBarStyle),
  SetTrafficLightPosition(Position),
  SetTheme(Option<Theme>),
  SetBackgroundColor(Option<Color>),
  DragWindow,
  ResizeDragWindow(tauri_runtime::ResizeDirection),
  RequestRedraw,
}

#[derive(Debug, Clone)]
pub enum SynthesizedWindowEvent {
  Focused(bool),
  DragDrop(DragDropEvent),
}

impl From<SynthesizedWindowEvent> for WindowEventWrapper {
  fn from(event: SynthesizedWindowEvent) -> Self {
    let event = match event {
      SynthesizedWindowEvent::Focused(focused) => WindowEvent::Focused(focused),
      SynthesizedWindowEvent::DragDrop(event) => WindowEvent::DragDrop(event),
    };
    Self(Some(event))
  }
}

pub enum WebviewMessage {
  AddEventListener(WebviewEventId, Box<dyn Fn(&WebviewEvent) + Send>),
  #[cfg(not(all(feature = "tracing", not(target_os = "android"))))]
  EvaluateScript(String),
  #[cfg(all(feature = "tracing", not(target_os = "android")))]
  EvaluateScript(String, Sender<()>, tracing::Span),
  #[cfg(not(all(feature = "tracing", not(target_os = "android"))))]
  EvaluateScriptWithCallback(String, Box<dyn Fn(String) + Send + 'static>),
  #[cfg(all(feature = "tracing", not(target_os = "android")))]
  EvaluateScriptWithCallback(
    String,
    Box<dyn Fn(String) + Send + 'static>,
    Sender<()>,
    tracing::Span,
  ),
  CookiesForUrl(Url, Sender<Result<Vec<tauri_runtime::Cookie<'static>>>>),
  Cookies(Sender<Result<Vec<tauri_runtime::Cookie<'static>>>>),
  SetCookie(tauri_runtime::Cookie<'static>),
  DeleteCookie(tauri_runtime::Cookie<'static>),
  WebviewEvent(WebviewEvent),
  SynthesizedWindowEvent(SynthesizedWindowEvent),
  Navigate(Url),
  Reload,
  Print,
  Close,
  Show,
  Hide,
  SetPosition(Position),
  SetSize(Size),
  SetBounds(tauri_runtime::dpi::Rect),
  SetFocus,
  Reparent(WindowId, Sender<Result<()>>),
  SetAutoResize(bool),
  SetZoom(f64),
  SetBackgroundColor(Option<Color>),
  ClearAllBrowsingData,
  // Getters
  Url(Sender<Result<String>>),
  Bounds(Sender<Result<tauri_runtime::dpi::Rect>>),
  Position(Sender<Result<PhysicalPosition<i32>>>),
  Size(Sender<Result<PhysicalSize<u32>>>),
  WithWebview(Box<dyn FnOnce(Webview) + Send>),
  // Devtools
  #[cfg(any(debug_assertions, feature = "devtools"))]
  OpenDevTools,
  #[cfg(any(debug_assertions, feature = "devtools"))]
  CloseDevTools,
  #[cfg(any(debug_assertions, feature = "devtools"))]
  IsDevToolsOpen(Sender<bool>),
}

pub enum EventLoopWindowTargetMessage {
  CursorPosition(Sender<Result<PhysicalPosition<f64>>>),
  SetTheme(Option<Theme>),
  SetDeviceEventFilter(DeviceEventFilter),
}

pub type CreateWindowClosure<T> =
  Box<dyn FnOnce(&EventLoopWindowTarget<Message<T>>) -> Result<WindowWrapper> + Send>;

pub type CreateWebviewClosure =
  Box<dyn FnOnce(&Window, CreateWebviewOptions) -> Result<WebviewWrapper> + Send>;

pub struct CreateWebviewOptions {
  pub focused_webview: Arc<Mutex<FocusState>>,
}

pub enum Message<T: 'static> {
  Task(Box<dyn FnOnce() + Send>),
  #[cfg(target_os = "macos")]
  SetActivationPolicy(ActivationPolicy),
  #[cfg(target_os = "macos")]
  SetDockVisibility(bool),
  RequestExit(i32),
  Application(ApplicationMessage),
  Window(WindowId, WindowMessage),
  Webview(WindowId, WebviewId, WebviewMessage),
  ServoWake(WindowId, WebviewId),
  EventLoopWindowTarget(EventLoopWindowTargetMessage),
  CreateWebview(WindowId, CreateWebviewClosure, Sender<Result<()>>),
  CreateWindow(WindowId, CreateWindowClosure<T>, Sender<Result<()>>),
  CreateRawWindow(
    WindowId,
    Box<dyn FnOnce() -> (String, TaoWindowBuilder) + Send>,
    Sender<Result<Weak<Window>>>,
  ),
  UserEvent(T),
}

impl<T: UserEvent> Clone for Message<T> {
  fn clone(&self) -> Self {
    match self {
      Self::UserEvent(t) => Self::UserEvent(t.clone()),
      _ => unimplemented!(),
    }
  }
}

/// The Tauri [`WebviewDispatch`] for [`Servo`].
#[derive(Debug, Clone)]
pub struct ServoWebviewDispatcher<T: UserEvent> {
  window_id: Arc<Mutex<WindowId>>,
  webview_id: WebviewId,
  context: Context<T>,
}

impl<T: UserEvent> WebviewDispatch<T> for ServoWebviewDispatcher<T> {
  type Runtime = Servo<T>;

  fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> Result<()> {
    send_user_message(&self.context, Message::Task(Box::new(f)))
  }

  fn on_webview_event<F: Fn(&WebviewEvent) + Send + 'static>(&self, f: F) -> WindowEventId {
    let id = self.context.next_webview_event_id();
    let _ = self.context.proxy.send_event(Message::Webview(
      *self.window_id.lock().unwrap(),
      self.webview_id,
      WebviewMessage::AddEventListener(id, Box::new(f)),
    ));
    id
  }

  fn with_webview<F: FnOnce(Box<dyn std::any::Any>) + Send + 'static>(&self, f: F) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::WithWebview(Box::new(move |webview| f(Box::new(webview)))),
      ),
    )
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  fn open_devtools(&self) {
    let _ = send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::OpenDevTools,
      ),
    );
  }

  #[cfg(any(debug_assertions, feature = "devtools"))]
  fn close_devtools(&self) {
    let _ = send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::CloseDevTools,
      ),
    );
  }

  /// Gets the devtools window's current open state.
  #[cfg(any(debug_assertions, feature = "devtools"))]
  fn is_devtools_open(&self) -> Result<bool> {
    webview_getter!(self, WebviewMessage::IsDevToolsOpen)
  }

  // Getters

  fn url(&self) -> Result<String> {
    webview_getter!(self, WebviewMessage::Url)?
  }

  fn bounds(&self) -> Result<tauri_runtime::dpi::Rect> {
    webview_getter!(self, WebviewMessage::Bounds)?
  }

  fn position(&self) -> Result<PhysicalPosition<i32>> {
    webview_getter!(self, WebviewMessage::Position)?
  }

  fn size(&self) -> Result<PhysicalSize<u32>> {
    webview_getter!(self, WebviewMessage::Size)?
  }

  // Setters

  fn navigate(&self, url: Url) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::Navigate(url),
      ),
    )
  }

  fn reload(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::Reload,
      ),
    )
  }

  fn print(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::Print,
      ),
    )
  }

  fn close(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::Close,
      ),
    )
  }

  fn set_bounds(&self, bounds: tauri_runtime::dpi::Rect) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetBounds(bounds),
      ),
    )
  }

  fn set_size(&self, size: Size) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetSize(size),
      ),
    )
  }

  fn set_position(&self, position: Position) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetPosition(position),
      ),
    )
  }

  fn set_focus(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetFocus,
      ),
    )
  }

  fn reparent(&self, window_id: WindowId) -> Result<()> {
    let mut current_window_id = self.window_id.lock().unwrap();
    let (tx, rx) = channel();
    send_user_message(
      &self.context,
      Message::Webview(
        *current_window_id,
        self.webview_id,
        WebviewMessage::Reparent(window_id, tx),
      ),
    )?;

    rx.recv().unwrap()?;

    *current_window_id = window_id;
    Ok(())
  }

  fn cookies_for_url(&self, url: Url) -> Result<Vec<Cookie<'static>>> {
    let (tx, rx) = channel();
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::CookiesForUrl(url, tx),
      ),
    )?;

    rx.recv().unwrap()
  }

  fn cookies(&self) -> Result<Vec<Cookie<'static>>> {
    webview_getter!(self, WebviewMessage::Cookies)?
  }

  fn set_cookie(&self, cookie: Cookie<'_>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetCookie(cookie.into_owned()),
      ),
    )?;
    Ok(())
  }

  fn delete_cookie(&self, cookie: Cookie<'_>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::DeleteCookie(cookie.into_owned()),
      ),
    )?;
    Ok(())
  }

  fn set_auto_resize(&self, auto_resize: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetAutoResize(auto_resize),
      ),
    )
  }

  #[cfg(all(feature = "tracing", not(target_os = "android")))]
  fn eval_script<S: Into<String>>(&self, script: S) -> Result<()> {
    // use a channel so the EvaluateScript task uses the current span as parent
    let (tx, rx) = channel();
    getter!(
      self,
      rx,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::EvaluateScript(script.into(), tx, tracing::Span::current()),
      )
    )
  }

  #[cfg(not(all(feature = "tracing", not(target_os = "android"))))]
  fn eval_script<S: Into<String>>(&self, script: S) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::EvaluateScript(script.into()),
      ),
    )
  }

  #[cfg(all(feature = "tracing", not(target_os = "android")))]
  fn eval_script_with_callback<S: Into<String>>(
    &self,
    script: S,
    callback: impl Fn(String) + Send + 'static,
  ) -> Result<()> {
    // use a channel so the EvaluateScript task uses the current span as parent
    let (tx, rx) = channel();
    getter!(
      self,
      rx,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::EvaluateScriptWithCallback(
          script.into(),
          Box::new(callback),
          tx,
          tracing::Span::current(),
        ),
      )
    )
  }

  #[cfg(not(all(feature = "tracing", not(target_os = "android"))))]
  fn eval_script_with_callback<S: Into<String>>(
    &self,
    script: S,
    callback: impl Fn(String) + Send + 'static,
  ) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::EvaluateScriptWithCallback(script.into(), Box::new(callback)),
      ),
    )
  }

  fn set_zoom(&self, scale_factor: f64) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetZoom(scale_factor),
      ),
    )
  }

  fn clear_all_browsing_data(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::ClearAllBrowsingData,
      ),
    )
  }

  fn hide(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::Hide,
      ),
    )
  }

  fn show(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::Show,
      ),
    )
  }

  fn set_background_color(&self, color: Option<Color>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Webview(
        *self.window_id.lock().unwrap(),
        self.webview_id,
        WebviewMessage::SetBackgroundColor(color),
      ),
    )
  }
}

/// The Tauri [`WindowDispatch`] for [`Servo`].
#[derive(Debug, Clone)]
pub struct ServoWindowDispatcher<T: UserEvent> {
  window_id: WindowId,
  context: Context<T>,
}

// SAFETY: this is safe since the `Context` usage is guarded on `send_user_message`.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: UserEvent> Sync for ServoWindowDispatcher<T> {}

fn get_raw_window_handle<T: UserEvent>(
  dispatcher: &ServoWindowDispatcher<T>,
) -> Result<std::result::Result<SendRawWindowHandle, raw_window_handle::HandleError>> {
  window_getter!(dispatcher, WindowMessage::RawWindowHandle)
}

impl<T: UserEvent> WindowDispatch<T> for ServoWindowDispatcher<T> {
  type Runtime = Servo<T>;
  type WindowBuilder = WindowBuilderWrapper;

  fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> Result<()> {
    send_user_message(&self.context, Message::Task(Box::new(f)))
  }

  fn on_window_event<F: Fn(&WindowEvent) + Send + 'static>(&self, f: F) -> WindowEventId {
    let id = self.context.next_window_event_id();
    let _ = self.context.proxy.send_event(Message::Window(
      self.window_id,
      WindowMessage::AddEventListener(id, Box::new(f)),
    ));
    id
  }

  // Getters

  fn scale_factor(&self) -> Result<f64> {
    window_getter!(self, WindowMessage::ScaleFactor)
  }

  fn inner_position(&self) -> Result<PhysicalPosition<i32>> {
    window_getter!(self, WindowMessage::InnerPosition)?
  }

  fn outer_position(&self) -> Result<PhysicalPosition<i32>> {
    window_getter!(self, WindowMessage::OuterPosition)?
  }

  fn inner_size(&self) -> Result<PhysicalSize<u32>> {
    window_getter!(self, WindowMessage::InnerSize)
  }

  fn outer_size(&self) -> Result<PhysicalSize<u32>> {
    window_getter!(self, WindowMessage::OuterSize)
  }

  fn is_fullscreen(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsFullscreen)
  }

  fn is_minimized(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsMinimized)
  }

  fn is_maximized(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsMaximized)
  }

  fn is_focused(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsFocused)
  }

  /// Gets the window's current decoration state.
  fn is_decorated(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsDecorated)
  }

  /// Gets the window's current resizable state.
  fn is_resizable(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsResizable)
  }

  /// Gets the current native window's maximize button state
  fn is_maximizable(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsMaximizable)
  }

  /// Gets the current native window's minimize button state
  fn is_minimizable(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsMinimizable)
  }

  /// Gets the current native window's close button state
  fn is_closable(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsClosable)
  }

  fn is_visible(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsVisible)
  }

  fn title(&self) -> Result<String> {
    window_getter!(self, WindowMessage::Title)
  }

  fn current_monitor(&self) -> Result<Option<Monitor>> {
    Ok(window_getter!(self, WindowMessage::CurrentMonitor)?.map(|m| MonitorHandleWrapper(m).into()))
  }

  fn primary_monitor(&self) -> Result<Option<Monitor>> {
    Ok(window_getter!(self, WindowMessage::PrimaryMonitor)?.map(|m| MonitorHandleWrapper(m).into()))
  }

  fn monitor_from_point(&self, x: f64, y: f64) -> Result<Option<Monitor>> {
    let (tx, rx) = channel();

    let _ = send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::MonitorFromPoint(tx, (x, y))),
    );

    Ok(
      rx.recv()
        .map_err(|_| crate::Error::FailedToReceiveMessage)?
        .map(|m| MonitorHandleWrapper(m).into()),
    )
  }

  fn available_monitors(&self) -> Result<Vec<Monitor>> {
    Ok(
      window_getter!(self, WindowMessage::AvailableMonitors)?
        .into_iter()
        .map(|m| MonitorHandleWrapper(m).into())
        .collect(),
    )
  }

  fn theme(&self) -> Result<Theme> {
    window_getter!(self, WindowMessage::Theme)
  }

  fn is_enabled(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsEnabled)
  }

  fn is_always_on_top(&self) -> Result<bool> {
    window_getter!(self, WindowMessage::IsAlwaysOnTop)
  }

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn gtk_window(&self) -> Result<gtk::ApplicationWindow> {
    window_getter!(self, WindowMessage::GtkWindow).map(|w| w.0)
  }

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn default_vbox(&self) -> Result<gtk::Box> {
    window_getter!(self, WindowMessage::GtkBox).map(|w| w.0)
  }

  /// Returns the name of the Android activity associated with this window.
  #[cfg(target_os = "android")]
  fn activity_name(&self) -> Result<String> {
    window_getter!(self, WindowMessage::ActivityName)
  }

  /// Returns the identifier of the UIScene tied to this UIWindow.
  #[cfg(target_os = "ios")]
  fn scene_identifier(&self) -> Result<String> {
    window_getter!(self, WindowMessage::SceneIdentifier)
  }

  fn window_handle(
    &self,
  ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
    get_raw_window_handle(self)
      .map_err(|_| raw_window_handle::HandleError::Unavailable)
      .and_then(|r| r.map(|h| unsafe { raw_window_handle::WindowHandle::borrow_raw(h.0) }))
  }

  // Setters

  fn center(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::Center),
    )
  }

  fn request_user_attention(&self, request_type: Option<UserAttentionType>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::RequestUserAttention(request_type.map(Into::into)),
      ),
    )
  }

  // Creates a window by dispatching a message to the event loop.
  // Note that this must be called from a separate thread, otherwise the channel will introduce a deadlock.
  fn create_window<F: Fn(RawWindow) + Send + 'static>(
    &mut self,
    pending: PendingWindow<T, Self::Runtime>,
    after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Self::Runtime>> {
    self.context.create_window(pending, after_window_creation)
  }

  // Creates a webview by dispatching a message to the event loop.
  // Note that this must be called from a separate thread, otherwise the channel will introduce a deadlock.
  fn create_webview(
    &mut self,
    pending: PendingWebview<T, Self::Runtime>,
  ) -> Result<DetachedWebview<T, Self::Runtime>> {
    self.context.create_webview(self.window_id, pending)
  }

  fn set_resizable(&self, resizable: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetResizable(resizable)),
    )
  }

  fn set_enabled(&self, enabled: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetEnabled(enabled)),
    )
  }

  fn set_maximizable(&self, maximizable: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetMaximizable(maximizable)),
    )
  }

  fn set_minimizable(&self, minimizable: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetMinimizable(minimizable)),
    )
  }

  fn set_closable(&self, closable: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetClosable(closable)),
    )
  }

  fn set_title<S: Into<String>>(&self, title: S) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetTitle(title.into())),
    )
  }

  fn maximize(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::Maximize),
    )
  }

  fn unmaximize(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::Unmaximize),
    )
  }

  fn minimize(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::Minimize),
    )
  }

  fn unminimize(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::Unminimize),
    )
  }

  fn show(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::Show),
    )
  }

  fn hide(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::Hide),
    )
  }

  fn close(&self) -> Result<()> {
    // NOTE: close cannot use the `send_user_message` function because it accesses the event loop callback
    self
      .context
      .proxy
      .send_event(Message::Window(self.window_id, WindowMessage::Close))
      .map_err(|_| Error::FailedToSendMessage)
  }

  fn destroy(&self) -> Result<()> {
    // NOTE: destroy cannot use the `send_user_message` function because it accesses the event loop callback
    self
      .context
      .proxy
      .send_event(Message::Window(self.window_id, WindowMessage::Destroy))
      .map_err(|_| Error::FailedToSendMessage)
  }

  fn set_decorations(&self, decorations: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetDecorations(decorations)),
    )
  }

  fn set_shadow(&self, enable: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetShadow(enable)),
    )
  }

  fn set_always_on_bottom(&self, always_on_bottom: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetAlwaysOnBottom(always_on_bottom),
      ),
    )
  }

  fn set_always_on_top(&self, always_on_top: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetAlwaysOnTop(always_on_top)),
    )
  }

  fn set_visible_on_all_workspaces(&self, visible_on_all_workspaces: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetVisibleOnAllWorkspaces(visible_on_all_workspaces),
      ),
    )
  }

  fn set_content_protected(&self, protected: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetContentProtected(protected),
      ),
    )
  }

  fn set_size(&self, size: Size) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetSize(size)),
    )
  }

  fn set_min_size(&self, size: Option<Size>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetMinSize(size)),
    )
  }

  fn set_max_size(&self, size: Option<Size>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetMaxSize(size)),
    )
  }

  fn set_size_constraints(&self, constraints: WindowSizeConstraints) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetSizeConstraints(constraints),
      ),
    )
  }

  fn set_position(&self, position: Position) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetPosition(position)),
    )
  }

  fn set_fullscreen(&self, fullscreen: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetFullscreen(fullscreen)),
    )
  }

  #[cfg(target_os = "macos")]
  fn set_simple_fullscreen(&self, enable: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetSimpleFullscreen(enable)),
    )
  }

  fn set_focus(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetFocus),
    )
  }

  fn set_focusable(&self, focusable: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetFocusable(focusable)),
    )
  }

  fn set_icon(&self, icon: Icon) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetIcon(TaoIcon::try_from(icon)?.0),
      ),
    )
  }

  fn set_skip_taskbar(&self, skip: bool) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetSkipTaskbar(skip)),
    )
  }

  fn set_cursor_grab(&self, grab: bool) -> crate::Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetCursorGrab(grab)),
    )
  }

  fn set_cursor_visible(&self, visible: bool) -> crate::Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetCursorVisible(visible)),
    )
  }

  fn set_cursor_icon(&self, icon: CursorIcon) -> crate::Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetCursorIcon(icon)),
    )
  }

  fn set_cursor_position<Pos: Into<Position>>(&self, position: Pos) -> crate::Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetCursorPosition(position.into()),
      ),
    )
  }

  fn set_ignore_cursor_events(&self, ignore: bool) -> crate::Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetIgnoreCursorEvents(ignore)),
    )
  }

  fn start_dragging(&self) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::DragWindow),
    )
  }

  fn start_resize_dragging(&self, direction: tauri_runtime::ResizeDirection) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::ResizeDragWindow(direction)),
    )
  }

  fn set_badge_count(&self, count: Option<i64>, desktop_filename: Option<String>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetBadgeCount(count, desktop_filename),
      ),
    )
  }

  fn set_badge_label(&self, label: Option<String>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetBadgeLabel(label)),
    )
  }

  fn set_overlay_icon(&self, icon: Option<Icon>) -> Result<()> {
    let icon: Result<Option<TaoIcon>> = icon.map_or(Ok(None), |x| Ok(Some(TaoIcon::try_from(x)?)));

    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetOverlayIcon(icon?)),
    )
  }

  fn set_progress_bar(&self, progress_state: ProgressBarState) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetProgressBar(progress_state),
      ),
    )
  }

  fn set_title_bar_style(&self, style: tauri_utils::TitleBarStyle) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetTitleBarStyle(style)),
    )
  }

  fn set_traffic_light_position(&self, position: Position) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(
        self.window_id,
        WindowMessage::SetTrafficLightPosition(position),
      ),
    )
  }

  fn set_theme(&self, theme: Option<Theme>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetTheme(theme)),
    )
  }

  fn set_background_color(&self, color: Option<Color>) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Window(self.window_id, WindowMessage::SetBackgroundColor(color)),
    )
  }
}

#[derive(Clone)]
pub struct WebviewWrapper {
  label: String,
  id: WebviewId,
  inner: Rc<WebView>,
  webview_event_listeners: WebviewEventListeners,
  bounds: Arc<Mutex<Option<WebviewBounds>>>,
}

impl Deref for WebviewWrapper {
  type Target = WebView;

  #[inline(always)]
  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

#[derive(Debug)]
pub enum FocusState {
  WindowFocused,
  WebviewFocused {
    webview_label: String,
  },
  Blured {
    last_focused_webview_label: Option<String>,
  },
}

impl Default for FocusState {
  fn default() -> Self {
    Self::Blured {
      last_focused_webview_label: None,
    }
  }
}

pub struct WindowWrapper {
  label: String,
  inner: Option<Arc<Window>>,
  // whether this window has child webviews
  // or it's just a container for a single webview
  has_children: AtomicBool,
  webviews: Vec<WebviewWrapper>,
  window_event_listeners: WindowEventListeners,
  #[cfg(windows)]
  background_color: Option<tao::window::RGBA>,
  #[cfg(windows)]
  is_window_transparent: bool,
  #[cfg(windows)]
  surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
  focused_webview: Arc<Mutex<FocusState>>,
}

impl WindowWrapper {
  pub fn label(&self) -> &str {
    &self.label
  }
}

impl fmt::Debug for WindowWrapper {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("WindowWrapper")
      .field("label", &self.label)
      .field("inner", &self.inner)
      .finish()
  }
}

#[derive(Debug, Clone)]
pub struct EventProxy<T: UserEvent>(TaoEventLoopProxy<Message<T>>);

#[cfg(target_os = "ios")]
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: UserEvent> Sync for EventProxy<T> {}

impl<T: UserEvent> EventLoopProxy<T> for EventProxy<T> {
  fn send_event(&self, event: T) -> Result<()> {
    self
      .0
      .send_event(Message::UserEvent(event))
      .map_err(|_| Error::EventLoopClosed)
  }
}

/// A Tauri [`Runtime`] wrapper around Servo.
pub struct Servo<T: UserEvent> {
  context: Context<T>,
  event_loop: EventLoop<Message<T>>,
}

impl<T: UserEvent> fmt::Debug for Servo<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Servo")
      .field("main_thread_id", &self.context.main_thread_id)
      .field("event_loop", &self.event_loop)
      .field("windows", &self.context.main_thread.windows)
      .finish()
  }
}

/// A handle to the Servo runtime.
#[derive(Debug, Clone)]
pub struct ServoHandle<T: UserEvent> {
  context: Context<T>,
}

// SAFETY: this is safe since the `Context` usage is guarded on `send_user_message`.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: UserEvent> Sync for ServoHandle<T> {}

impl<T: UserEvent> ServoHandle<T> {
  /// Creates a new tao window using a callback, and returns its window id.
  pub fn create_tao_window<F: FnOnce() -> (String, TaoWindowBuilder) + Send + 'static>(
    &self,
    f: F,
  ) -> Result<Weak<Window>> {
    let id = self.context.next_window_id();
    let (tx, rx) = channel();
    send_user_message(&self.context, Message::CreateRawWindow(id, Box::new(f), tx))?;
    rx.recv().unwrap()
  }

  /// Gets the [`WindowId`] associated with the given [`TaoWindowId`].
  pub fn window_id(&self, window_id: TaoWindowId) -> WindowId {
    self.context.window_id_map.get(&window_id).unwrap()
  }

  /// Send a message to the event loop.
  pub fn send_event(&self, message: Message<T>) -> Result<()> {
    self
      .context
      .proxy
      .send_event(message)
      .map_err(|_| Error::FailedToSendMessage)?;
    Ok(())
  }
}

impl<T: UserEvent> RuntimeHandle<T> for ServoHandle<T> {
  type Runtime = Servo<T>;

  fn create_proxy(&self) -> EventProxy<T> {
    EventProxy(self.context.proxy.clone())
  }

  #[cfg(target_os = "macos")]
  fn set_activation_policy(&self, activation_policy: ActivationPolicy) -> Result<()> {
    send_user_message(
      &self.context,
      Message::SetActivationPolicy(activation_policy),
    )
  }

  #[cfg(target_os = "macos")]
  fn set_dock_visibility(&self, visible: bool) -> Result<()> {
    send_user_message(&self.context, Message::SetDockVisibility(visible))
  }

  fn request_exit(&self, code: i32) -> Result<()> {
    // NOTE: request_exit cannot use the `send_user_message` function because it accesses the event loop callback
    self
      .context
      .proxy
      .send_event(Message::RequestExit(code))
      .map_err(|_| Error::FailedToSendMessage)
  }

  // Creates a window by dispatching a message to the event loop.
  // Note that this must be called from a separate thread, otherwise the channel will introduce a deadlock.
  fn create_window<F: Fn(RawWindow) + Send + 'static>(
    &self,
    pending: PendingWindow<T, Self::Runtime>,
    after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Self::Runtime>> {
    self.context.create_window(pending, after_window_creation)
  }

  // Creates a webview by dispatching a message to the event loop.
  // Note that this must be called from a separate thread, otherwise the channel will introduce a deadlock.
  fn create_webview(
    &self,
    window_id: WindowId,
    pending: PendingWebview<T, Self::Runtime>,
  ) -> Result<DetachedWebview<T, Self::Runtime>> {
    self.context.create_webview(window_id, pending)
  }

  fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> Result<()> {
    send_user_message(&self.context, Message::Task(Box::new(f)))
  }

  fn display_handle(
    &self,
  ) -> std::result::Result<DisplayHandle<'_>, raw_window_handle::HandleError> {
    self.context.main_thread.window_target.display_handle()
  }

  fn primary_monitor(&self) -> Option<Monitor> {
    self
      .context
      .main_thread
      .window_target
      .primary_monitor()
      .map(|m| MonitorHandleWrapper(m).into())
  }

  fn monitor_from_point(&self, x: f64, y: f64) -> Option<Monitor> {
    self
      .context
      .main_thread
      .window_target
      .monitor_from_point(x, y)
      .map(|m| MonitorHandleWrapper(m).into())
  }

  fn available_monitors(&self) -> Vec<Monitor> {
    self
      .context
      .main_thread
      .window_target
      .available_monitors()
      .map(|m| MonitorHandleWrapper(m).into())
      .collect()
  }

  fn cursor_position(&self) -> Result<PhysicalPosition<f64>> {
    event_loop_window_getter!(self, EventLoopWindowTargetMessage::CursorPosition)?
      .map_err(|_| Error::FailedToGetCursorPosition)
  }

  fn set_theme(&self, theme: Option<Theme>) {
    let _ = send_user_message(
      &self.context,
      Message::EventLoopWindowTarget(EventLoopWindowTargetMessage::SetTheme(theme)),
    );
  }

  #[cfg(target_os = "macos")]
  fn show(&self) -> tauri_runtime::Result<()> {
    send_user_message(
      &self.context,
      Message::Application(ApplicationMessage::Show),
    )
  }

  #[cfg(target_os = "macos")]
  fn hide(&self) -> tauri_runtime::Result<()> {
    send_user_message(
      &self.context,
      Message::Application(ApplicationMessage::Hide),
    )
  }

  fn set_device_event_filter(&self, filter: DeviceEventFilter) {
    let _ = send_user_message(
      &self.context,
      Message::EventLoopWindowTarget(EventLoopWindowTargetMessage::SetDeviceEventFilter(filter)),
    );
  }

  #[cfg(target_os = "android")]
  fn find_class<'a>(
    &self,
    env: &mut jni::JNIEnv<'a>,
    activity: &jni::objects::JObject<'_>,
    name: impl Into<String>,
  ) -> std::result::Result<jni::objects::JClass<'a>, jni::errors::Error> {
    find_class(env, activity, name.into())
  }

  #[cfg(target_os = "android")]
  fn run_on_android_context<F>(&self, f: F)
  where
    F: FnOnce(&mut jni::JNIEnv<'_>, &jni::objects::JObject<'_>, &jni::objects::JObject<'_>)
      + Send
      + 'static,
  {
    dispatch(f)
  }

  #[cfg(any(target_os = "macos", target_os = "ios"))]
  fn fetch_data_store_identifiers<F: FnOnce(Vec<[u8; 16]>) + Send + 'static>(
    &self,
    cb: F,
  ) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Application(ApplicationMessage::FetchDataStoreIdentifiers(Box::new(cb))),
    )
  }

  #[cfg(any(target_os = "macos", target_os = "ios"))]
  fn remove_data_store<F: FnOnce(Result<()>) + Send + 'static>(
    &self,
    uuid: [u8; 16],
    cb: F,
  ) -> Result<()> {
    send_user_message(
      &self.context,
      Message::Application(ApplicationMessage::RemoveDataStore(uuid, Box::new(cb))),
    )
  }
}

impl<T: UserEvent> Servo<T> {
  fn init_with_builder(
    mut event_loop_builder: EventLoopBuilder<Message<T>>,
    #[allow(unused_variables)] args: RuntimeInitArgs,
  ) -> Result<Self> {
    #[cfg(windows)]
    if let Some(hook) = args.msg_hook {
      use tao::platform::windows::EventLoopBuilderExtWindows;
      event_loop_builder.with_msg_hook(hook);
    }

    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    if let Some(app_id) = args.app_id {
      use tao::platform::unix::EventLoopBuilderExtUnix;
      event_loop_builder.with_app_id(app_id);
    }
    Self::init(event_loop_builder.build())
  }

  fn init(event_loop: EventLoop<Message<T>>) -> Result<Self> {
    install_rustls_crypto_provider();
    let main_thread_id = current_thread().id();

    #[allow(clippy::arc_with_non_send_sync)]
    let windows = Arc::new(WindowsStore(RefCell::new(BTreeMap::default())));
    let window_id_map = WindowIdStore::default();

    let context = Context {
      window_id_map,
      main_thread_id,
      proxy: event_loop.create_proxy(),
      main_thread: DispatcherMainThreadContext {
        window_target: event_loop.deref().clone(),
        windows,
        #[cfg(feature = "tracing")]
        active_tracing_spans: Default::default(),
      },
      next_window_id: Default::default(),
      next_webview_id: Default::default(),
      next_window_event_id: Default::default(),
      next_webview_event_id: Default::default(),
    };

    Ok(Self {
      context,
      event_loop,
    })
  }
}

/// Installs a default rustls [`CryptoProvider`] so Servo's network stack can do TLS.
///
/// Servo's dependencies enable the `aws-lc-rs` provider, but feature unification
/// can leave rustls without an unambiguous crate-feature default; installing the
/// provider explicitly avoids that panic.
fn install_rustls_crypto_provider() {
  use std::sync::Once;

  static INIT: Once = Once::new();
  INIT.call_once(|| {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
  });
}

impl<T: UserEvent> Runtime<T> for Servo<T> {
  type WindowDispatcher = ServoWindowDispatcher<T>;
  type WebviewDispatcher = ServoWebviewDispatcher<T>;
  type Handle = ServoHandle<T>;

  type EventLoopProxy = EventProxy<T>;

  fn new(args: RuntimeInitArgs) -> Result<Self> {
    Self::init_with_builder(EventLoopBuilder::<Message<T>>::with_user_event(), args)
  }
  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn new_any_thread(args: RuntimeInitArgs) -> Result<Self> {
    use tao::platform::unix::EventLoopBuilderExtUnix;
    let mut event_loop_builder = EventLoopBuilder::<Message<T>>::with_user_event();
    event_loop_builder.with_any_thread(true);
    Self::init_with_builder(event_loop_builder, args)
  }

  #[cfg(windows)]
  fn new_any_thread(args: RuntimeInitArgs) -> Result<Self> {
    use tao::platform::windows::EventLoopBuilderExtWindows;
    let mut event_loop_builder = EventLoopBuilder::<Message<T>>::with_user_event();
    event_loop_builder.with_any_thread(true);
    Self::init_with_builder(event_loop_builder, args)
  }

  fn create_proxy(&self) -> EventProxy<T> {
    EventProxy(self.event_loop.create_proxy())
  }

  fn handle(&self) -> Self::Handle {
    ServoHandle {
      context: self.context.clone(),
    }
  }

  fn create_window<F: Fn(RawWindow) + Send + 'static>(
    &self,
    pending: PendingWindow<T, Self>,
    after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Self>> {
    let label = pending.label.clone();
    let window_id = self.context.next_window_id();
    let (webview_id, use_https_scheme) = pending
      .webview
      .as_ref()
      .map(|w| {
        (
          Some(self.context.next_webview_id()),
          w.webview_attributes.use_https_scheme,
        )
      })
      .unwrap_or((None, false));

    let window = create_window(
      window_id,
      webview_id.unwrap_or_default(),
      &self.event_loop,
      &self.context,
      pending,
      after_window_creation,
    )?;

    let dispatcher = ServoWindowDispatcher {
      window_id,
      context: self.context.clone(),
    };

    self
      .context
      .main_thread
      .windows
      .0
      .borrow_mut()
      .insert(window_id, window);

    let detached_webview = webview_id.map(|id| {
      let webview = DetachedWebview {
        label: label.clone(),
        dispatcher: ServoWebviewDispatcher {
          window_id: Arc::new(Mutex::new(window_id)),
          webview_id: id,
          context: self.context.clone(),
        },
      };
      DetachedWindowWebview {
        webview,
        use_https_scheme,
      }
    });

    Ok(DetachedWindow {
      id: window_id,
      label,
      dispatcher,
      webview: detached_webview,
    })
  }

  fn create_webview(
    &self,
    window_id: WindowId,
    pending: PendingWebview<T, Self>,
  ) -> Result<DetachedWebview<T, Self>> {
    let label = pending.label.clone();

    let window = self
      .context
      .main_thread
      .windows
      .0
      .borrow()
      .get(&window_id)
      .map(|w| (w.inner.clone(), w.focused_webview.clone()));
    if let Some((Some(window), focused_webview)) = window {
      let window_id_wrapper = Arc::new(Mutex::new(window_id));

      let webview_id = self.context.next_webview_id();

      let webview = create_webview(
        WebviewKind::WindowChild,
        &window,
        window_id_wrapper.clone(),
        webview_id,
        &self.context,
        pending,
        focused_webview,
      )?;

      if let Some(w) = self
        .context
        .main_thread
        .windows
        .0
        .borrow_mut()
        .get_mut(&window_id)
      {
        w.webviews.push(webview);
        w.has_children.store(true, Ordering::Relaxed);
      }

      let dispatcher = ServoWebviewDispatcher {
        window_id: window_id_wrapper,
        webview_id,
        context: self.context.clone(),
      };

      Ok(DetachedWebview { label, dispatcher })
    } else {
      Err(Error::WindowNotFound)
    }
  }

  fn primary_monitor(&self) -> Option<Monitor> {
    self
      .context
      .main_thread
      .window_target
      .primary_monitor()
      .map(|m| MonitorHandleWrapper(m).into())
  }

  fn monitor_from_point(&self, x: f64, y: f64) -> Option<Monitor> {
    self
      .context
      .main_thread
      .window_target
      .monitor_from_point(x, y)
      .map(|m| MonitorHandleWrapper(m).into())
  }

  fn available_monitors(&self) -> Vec<Monitor> {
    self
      .context
      .main_thread
      .window_target
      .available_monitors()
      .map(|m| MonitorHandleWrapper(m).into())
      .collect()
  }

  fn cursor_position(&self) -> Result<PhysicalPosition<f64>> {
    self
      .context
      .main_thread
      .window_target
      .cursor_position()
      .map_err(|_| Error::FailedToGetCursorPosition)
  }

  fn set_theme(&self, theme: Option<Theme>) {
    self.event_loop.set_theme(to_tao_theme(theme));
  }

  #[cfg(target_os = "macos")]
  fn set_activation_policy(&mut self, activation_policy: ActivationPolicy) {
    self
      .event_loop
      .set_activation_policy(tao_activation_policy(activation_policy));
  }

  #[cfg(target_os = "macos")]
  fn set_dock_visibility(&mut self, visible: bool) {
    self.event_loop.set_dock_visibility(visible);
  }

  #[cfg(target_os = "macos")]
  fn show(&self) {
    self.event_loop.show_application();
  }

  #[cfg(target_os = "macos")]
  fn hide(&self) {
    self.event_loop.hide_application();
  }

  fn set_device_event_filter(&mut self, filter: DeviceEventFilter) {
    self
      .event_loop
      .set_device_event_filter(DeviceEventFilterWrapper::from(filter).0);
  }

  #[cfg(desktop)]
  fn run_iteration<F: FnMut(RunEvent<T>) + 'static>(&mut self, mut callback: F) {
    use tao::platform::run_return::EventLoopExtRunReturn;
    let windows = &self.context.main_thread.windows;
    let window_id_map = &self.context.window_id_map;

    #[cfg(feature = "tracing")]
    let active_tracing_spans = &self.context.main_thread.active_tracing_spans;

    self
      .event_loop
      .run_return(|event, event_loop, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::MainEventsCleared = &event {
          *control_flow = ControlFlow::Exit;
        }

        handle_event_loop(
          event,
          event_loop,
          control_flow,
          &mut callback,
          window_id_map,
          windows,
          #[cfg(feature = "tracing")]
          active_tracing_spans,
        );
      });
  }

  fn run<F: FnMut(RunEvent<T>) + 'static>(self, callback: F) {
    let event_handler = make_event_handler(self.context, callback);
    self.event_loop.run(event_handler)
  }

  #[cfg(not(target_os = "ios"))]
  fn run_return<F: FnMut(RunEvent<T>) + 'static>(mut self, callback: F) -> i32 {
    use tao::platform::run_return::EventLoopExtRunReturn;

    let event_handler = make_event_handler(self.context, callback);
    self.event_loop.run_return(event_handler)
  }

  #[cfg(target_os = "ios")]
  fn run_return<F: FnMut(RunEvent<T>) + 'static>(self, callback: F) -> i32 {
    self.run(callback);
    0
  }
}

fn make_event_handler<T: UserEvent, F: FnMut(RunEvent<T>) + 'static>(
  context: Context<T>,
  mut callback: F,
) -> impl FnMut(Event<'_, Message<T>>, &EventLoopWindowTarget<Message<T>>, &mut ControlFlow) {
  let windows = context.main_thread.windows;
  let window_id_map = context.window_id_map;

  #[cfg(feature = "tracing")]
  let active_tracing_spans = context.main_thread.active_tracing_spans;

  move |event, event_loop, control_flow| {
    handle_event_loop(
      event,
      event_loop,
      control_flow,
      &mut callback,
      &window_id_map,
      &windows,
      #[cfg(feature = "tracing")]
      &active_tracing_spans,
    );
  }
}

struct UserMessageContext<'a> {
  windows: &'a WindowsStore,
  window_id_map: &'a WindowIdStore,
}

fn handle_user_message<T: UserEvent>(
  event_loop: &EventLoopWindowTarget<Message<T>>,
  message: Message<T>,
  context: UserMessageContext,
) {
  let UserMessageContext {
    window_id_map,
    windows,
  } = context;
  match message {
    Message::Task(task) => task(),
    #[cfg(target_os = "macos")]
    Message::SetActivationPolicy(activation_policy) => {
      event_loop.set_activation_policy_at_runtime(tao_activation_policy(activation_policy))
    }
    #[cfg(target_os = "macos")]
    Message::SetDockVisibility(visible) => event_loop.set_dock_visibility(visible),
    Message::RequestExit(_code) => panic!("cannot handle RequestExit on the main thread"),
    Message::Application(application_message) => match application_message {
      #[cfg(target_os = "macos")]
      ApplicationMessage::Show => {
        event_loop.show_application();
      }
      #[cfg(target_os = "macos")]
      ApplicationMessage::Hide => {
        event_loop.hide_application();
      }
      #[cfg(any(target_os = "macos", target_os = "ios"))]
      ApplicationMessage::FetchDataStoreIdentifiers(cb) => cb(Vec::new()),
      #[cfg(any(target_os = "macos", target_os = "ios"))]
      ApplicationMessage::RemoveDataStore(_, cb) => cb(Err(Error::FailedToRemoveDataStore)),
    },
    Message::Window(id, window_message) => {
      let w = windows.0.borrow().get(&id).map(|w| {
        (
          w.inner.clone(),
          w.webviews.clone(),
          w.has_children.load(Ordering::Relaxed),
          w.window_event_listeners.clone(),
        )
      });
      if let Some((Some(window), webviews, has_children, window_event_listeners)) = w {
        match window_message {
          WindowMessage::AddEventListener(id, listener) => {
            window_event_listeners.lock().unwrap().insert(id, listener);
          }

          // Getters
          WindowMessage::ScaleFactor(tx) => tx.send(window.scale_factor()).unwrap(),
          WindowMessage::InnerPosition(tx) => tx
            .send(
              window
                .inner_position()
                .map_err(|_| Error::FailedToSendMessage),
            )
            .unwrap(),
          WindowMessage::OuterPosition(tx) => tx
            .send(
              window
                .outer_position()
                .map_err(|_| Error::FailedToSendMessage),
            )
            .unwrap(),
          WindowMessage::InnerSize(tx) => tx
            .send(inner_size(&window, &webviews, has_children))
            .unwrap(),
          WindowMessage::OuterSize(tx) => tx.send(window.outer_size()).unwrap(),
          WindowMessage::IsFullscreen(tx) => tx.send(window.fullscreen().is_some()).unwrap(),
          WindowMessage::IsMinimized(tx) => tx.send(window.is_minimized()).unwrap(),
          WindowMessage::IsMaximized(tx) => tx.send(window.is_maximized()).unwrap(),
          WindowMessage::IsFocused(tx) => tx.send(window.is_focused()).unwrap(),
          WindowMessage::IsDecorated(tx) => tx.send(window.is_decorated()).unwrap(),
          WindowMessage::IsResizable(tx) => tx.send(window.is_resizable()).unwrap(),
          WindowMessage::IsMaximizable(tx) => tx.send(window.is_maximizable()).unwrap(),
          WindowMessage::IsMinimizable(tx) => tx.send(window.is_minimizable()).unwrap(),
          WindowMessage::IsClosable(tx) => tx.send(window.is_closable()).unwrap(),
          WindowMessage::IsVisible(tx) => tx.send(window.is_visible()).unwrap(),
          WindowMessage::Title(tx) => tx.send(window.title()).unwrap(),
          WindowMessage::CurrentMonitor(tx) => tx.send(window.current_monitor()).unwrap(),
          WindowMessage::PrimaryMonitor(tx) => tx.send(window.primary_monitor()).unwrap(),
          WindowMessage::MonitorFromPoint(tx, (x, y)) => {
            tx.send(window.monitor_from_point(x, y)).unwrap()
          }
          WindowMessage::AvailableMonitors(tx) => {
            tx.send(window.available_monitors().collect()).unwrap()
          }
          #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
          ))]
          WindowMessage::GtkWindow(tx) => tx.send(GtkWindow(window.gtk_window().clone())).unwrap(),
          #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
          ))]
          WindowMessage::GtkBox(tx) => tx
            .send(GtkBox(window.default_vbox().unwrap().clone()))
            .unwrap(),
          #[cfg(target_os = "android")]
          WindowMessage::ActivityName(tx) => {
            tx.send(window.activity_name()).unwrap();
          }
          #[cfg(target_os = "ios")]
          WindowMessage::SceneIdentifier(tx) => {
            tx.send(window.scene_identifier()).unwrap();
          }
          WindowMessage::RawWindowHandle(tx) => tx
            .send(
              window
                .window_handle()
                .map(|h| SendRawWindowHandle(h.as_raw())),
            )
            .unwrap(),
          WindowMessage::Theme(tx) => {
            tx.send(map_theme(&window.theme())).unwrap();
          }
          WindowMessage::IsEnabled(tx) => tx.send(window.is_enabled()).unwrap(),
          WindowMessage::IsAlwaysOnTop(tx) => tx.send(window.is_always_on_top()).unwrap(),
          // Setters
          WindowMessage::Center => window.center(),
          WindowMessage::RequestUserAttention(request_type) => {
            window.request_user_attention(request_type.map(|r| r.0));
          }
          WindowMessage::SetResizable(resizable) => {
            window.set_resizable(resizable);
            #[cfg(windows)]
            if !resizable {
              undecorated_resizing::detach_resize_handler(window.hwnd());
            } else if !window.is_decorated() {
              undecorated_resizing::attach_resize_handler(
                window.hwnd(),
                window.has_undecorated_shadow(),
              );
            }
          }
          WindowMessage::SetMaximizable(maximizable) => window.set_maximizable(maximizable),
          WindowMessage::SetMinimizable(minimizable) => window.set_minimizable(minimizable),
          WindowMessage::SetClosable(closable) => window.set_closable(closable),
          WindowMessage::SetTitle(title) => window.set_title(&title),
          WindowMessage::Maximize => window.set_maximized(true),
          WindowMessage::Unmaximize => window.set_maximized(false),
          WindowMessage::Minimize => window.set_minimized(true),
          WindowMessage::Unminimize => window.set_minimized(false),
          WindowMessage::SetEnabled(enabled) => window.set_enabled(enabled),
          WindowMessage::Show => window.set_visible(true),
          WindowMessage::Hide => window.set_visible(false),
          WindowMessage::Close => {
            panic!("cannot handle `WindowMessage::Close` on the main thread")
          }
          WindowMessage::Destroy => {
            panic!("cannot handle `WindowMessage::Destroy` on the main thread")
          }
          WindowMessage::SetDecorations(decorations) => {
            window.set_decorations(decorations);
            #[cfg(windows)]
            if decorations {
              undecorated_resizing::detach_resize_handler(window.hwnd());
            } else if window.is_resizable() {
              undecorated_resizing::attach_resize_handler(
                window.hwnd(),
                window.has_undecorated_shadow(),
              );
            }
          }
          WindowMessage::SetShadow(_enable) => {
            #[cfg(windows)]
            {
              window.set_undecorated_shadow(_enable);
              undecorated_resizing::update_drag_hwnd_rgn_for_undecorated(window.hwnd(), _enable);
            }
            #[cfg(target_os = "macos")]
            window.set_has_shadow(_enable);
          }
          WindowMessage::SetAlwaysOnBottom(always_on_bottom) => {
            window.set_always_on_bottom(always_on_bottom)
          }
          WindowMessage::SetAlwaysOnTop(always_on_top) => window.set_always_on_top(always_on_top),
          WindowMessage::SetVisibleOnAllWorkspaces(visible_on_all_workspaces) => {
            window.set_visible_on_all_workspaces(visible_on_all_workspaces)
          }
          WindowMessage::SetContentProtected(protected) => window.set_content_protection(protected),
          WindowMessage::SetSize(size) => {
            window.set_inner_size(size);
          }
          WindowMessage::SetMinSize(size) => {
            window.set_min_inner_size(size);
          }
          WindowMessage::SetMaxSize(size) => {
            window.set_max_inner_size(size);
          }
          WindowMessage::SetSizeConstraints(constraints) => {
            window.set_inner_size_constraints(tao::window::WindowSizeConstraints {
              min_width: constraints.min_width,
              min_height: constraints.min_height,
              max_width: constraints.max_width,
              max_height: constraints.max_height,
            });
          }
          WindowMessage::SetPosition(position) => window.set_outer_position(position),
          WindowMessage::SetFullscreen(fullscreen) => {
            if fullscreen {
              window.set_fullscreen(Some(Fullscreen::Borderless(None)))
            } else {
              window.set_fullscreen(None)
            }
          }

          #[cfg(target_os = "macos")]
          WindowMessage::SetSimpleFullscreen(enable) => {
            window.set_simple_fullscreen(enable);
          }

          WindowMessage::SetFocus => {
            window.set_focus();
          }
          WindowMessage::SetFocusable(focusable) => {
            window.set_focusable(focusable);
          }
          WindowMessage::SetIcon(icon) => {
            window.set_window_icon(Some(icon));
          }
          #[allow(unused_variables)]
          WindowMessage::SetSkipTaskbar(skip) => {
            #[cfg(any(
              windows,
              target_os = "linux",
              target_os = "dragonfly",
              target_os = "freebsd",
              target_os = "netbsd",
              target_os = "openbsd"
            ))]
            let _ = window.set_skip_taskbar(skip);
          }
          WindowMessage::SetCursorGrab(grab) => {
            let _ = window.set_cursor_grab(grab);
          }
          WindowMessage::SetCursorVisible(visible) => {
            window.set_cursor_visible(visible);
          }
          WindowMessage::SetCursorIcon(icon) => {
            window.set_cursor_icon(CursorIconWrapper::from(icon).0);
          }
          WindowMessage::SetCursorPosition(position) => {
            let _ = window.set_cursor_position(position);
          }
          WindowMessage::SetIgnoreCursorEvents(ignore) => {
            let _ = window.set_ignore_cursor_events(ignore);
          }
          WindowMessage::DragWindow => {
            let _ = window.drag_window();
          }
          WindowMessage::ResizeDragWindow(direction) => {
            let _ = window.drag_resize_window(match direction {
              tauri_runtime::ResizeDirection::East => tao::window::ResizeDirection::East,
              tauri_runtime::ResizeDirection::North => tao::window::ResizeDirection::North,
              tauri_runtime::ResizeDirection::NorthEast => tao::window::ResizeDirection::NorthEast,
              tauri_runtime::ResizeDirection::NorthWest => tao::window::ResizeDirection::NorthWest,
              tauri_runtime::ResizeDirection::South => tao::window::ResizeDirection::South,
              tauri_runtime::ResizeDirection::SouthEast => tao::window::ResizeDirection::SouthEast,
              tauri_runtime::ResizeDirection::SouthWest => tao::window::ResizeDirection::SouthWest,
              tauri_runtime::ResizeDirection::West => tao::window::ResizeDirection::West,
            });
          }
          WindowMessage::RequestRedraw => {
            window.request_redraw();
          }
          WindowMessage::SetBadgeCount(_count, _desktop_filename) => {
            #[cfg(target_os = "ios")]
            window.set_badge_count(
              _count.map_or(0, |x| x.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
            );

            #[cfg(target_os = "macos")]
            window.set_badge_label(_count.map(|x| x.to_string()));

            #[cfg(any(
              target_os = "linux",
              target_os = "dragonfly",
              target_os = "freebsd",
              target_os = "netbsd",
              target_os = "openbsd"
            ))]
            window.set_badge_count(_count, _desktop_filename);
          }
          WindowMessage::SetBadgeLabel(_label) => {
            #[cfg(target_os = "macos")]
            window.set_badge_label(_label);
          }
          WindowMessage::SetOverlayIcon(_icon) => {
            #[cfg(windows)]
            window.set_overlay_icon(_icon.map(|x| x.0).as_ref());
          }
          WindowMessage::SetProgressBar(progress_state) => {
            window.set_progress_bar(ProgressBarStateWrapper::from(progress_state).0);
          }
          WindowMessage::SetTitleBarStyle(_style) => {
            #[cfg(target_os = "macos")]
            match _style {
              TitleBarStyle::Visible => {
                window.set_titlebar_transparent(false);
                window.set_fullsize_content_view(true);
              }
              TitleBarStyle::Transparent => {
                window.set_titlebar_transparent(true);
                window.set_fullsize_content_view(false);
              }
              TitleBarStyle::Overlay => {
                window.set_titlebar_transparent(true);
                window.set_fullsize_content_view(true);
              }
              unknown => {
                #[cfg(feature = "tracing")]
                tracing::warn!("unknown title bar style applied: {unknown}");

                #[cfg(not(feature = "tracing"))]
                eprintln!("unknown title bar style applied: {unknown}");
              }
            };
          }
          WindowMessage::SetTrafficLightPosition(_position) => {
            #[cfg(target_os = "macos")]
            window.set_traffic_light_inset(_position);
          }
          WindowMessage::SetTheme(theme) => {
            window.set_theme(to_tao_theme(theme));
          }
          WindowMessage::SetBackgroundColor(color) => {
            #[cfg(windows)]
            if let Some(window_wrapper) = windows.0.borrow_mut().get_mut(&id) {
              window_wrapper.background_color = color.map(Into::into);
              if let Some(surface) = &mut window_wrapper.surface {
                window.draw_surface(surface, window_wrapper.background_color);
              }
            }
            window.set_background_color(color.map(Into::into))
          }
        }
      }
    }
    Message::Webview(window_id, webview_id, webview_message) => {
      #[cfg(any(
        target_os = "macos",
        windows,
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      if let WebviewMessage::Reparent(new_parent_window_id, tx) = webview_message {
        let webview_handle = windows.0.borrow_mut().get_mut(&window_id).and_then(|w| {
          w.webviews
            .iter()
            .position(|w| w.id == webview_id)
            .map(|webview_index| w.webviews.remove(webview_index))
        });

        if let Some(webview) = webview_handle {
          if let Some((Some(new_parent_window), new_parent_window_webviews)) = windows
            .0
            .borrow_mut()
            .get_mut(&new_parent_window_id)
            .map(|w| (w.inner.clone(), &mut w.webviews))
          {
            // Servo's embedding API has no webview reparenting; a Servo
            // webview also composites into exactly one native window (see
            // README "Known limitations"), so this is a hard unsupported.
            let _ = &new_parent_window;
            let reparent_result: crate::ServoResult<()> = Err(crate::ServoError::Servo(
              "reparenting embedded Servo webviews is not supported".into(),
            ));

            match reparent_result {
              Ok(_) => {
                new_parent_window_webviews.push(webview);
                tx.send(Ok(())).unwrap();
              }
              Err(e) => {
                log::error!("failed to reparent webview: {e}");
                tx.send(Err(Error::FailedToSendMessage)).unwrap();
              }
            }
          }
        } else {
          tx.send(Err(Error::FailedToSendMessage)).unwrap();
        }

        return;
      }

      let webview_handle = windows.0.borrow().get(&window_id).map(|w| {
        (
          w.inner.clone(),
          w.webviews.iter().find(|w| w.id == webview_id).cloned(),
        )
      });
      if let Some((Some(window), Some(webview))) = webview_handle {
        match webview_message {
          WebviewMessage::WebviewEvent(_) => { /* already handled */ }
          WebviewMessage::SynthesizedWindowEvent(_) => { /* already handled */ }
          WebviewMessage::Reparent(_window_id, _tx) => { /* already handled */ }
          WebviewMessage::AddEventListener(id, listener) => {
            webview
              .webview_event_listeners
              .lock()
              .unwrap()
              .insert(id, listener);
          }

          #[cfg(all(feature = "tracing", not(target_os = "android")))]
          WebviewMessage::EvaluateScript(script, tx, span) => {
            let _span = span.entered();
            if let Err(e) = webview.evaluate_script(&script) {
              log::error!("{e}");
            }
            tx.send(()).unwrap();
          }
          #[cfg(not(all(feature = "tracing", not(target_os = "android"))))]
          WebviewMessage::EvaluateScript(script) => {
            if let Err(e) = webview.evaluate_script(&script) {
              log::error!("{e}");
            }
          }
          #[cfg(all(feature = "tracing", not(target_os = "android")))]
          WebviewMessage::EvaluateScriptWithCallback(script, callback, tx, span) => {
            let _span = span.entered();
            if let Err(e) = webview.evaluate_script_with_callback(&script, callback) {
              log::error!("{e}");
            }
            tx.send(()).unwrap();
          }
          #[cfg(not(all(feature = "tracing", not(target_os = "android"))))]
          WebviewMessage::EvaluateScriptWithCallback(script, callback) => {
            if let Err(e) = webview.evaluate_script_with_callback(&script, callback) {
              log::error!("{e}");
            }
          }
          WebviewMessage::Navigate(url) => {
            if let Err(e) = webview.load_url(url.as_str()) {
              log::error!("failed to navigate to url {}: {}", url, e);
            }
          }
          WebviewMessage::Reload => {
            if let Err(e) = webview.reload() {
              log::error!("failed to reload: {e}");
            }
          }
          WebviewMessage::Show => {
            if let Err(e) = webview.set_visible(true) {
              log::error!("failed to change webview visibility: {e}");
            }
          }
          WebviewMessage::Hide => {
            if let Err(e) = webview.set_visible(false) {
              log::error!("failed to change webview visibility: {e}");
            }
          }
          WebviewMessage::Print => {
            let _ = webview.print();
          }
          WebviewMessage::Close => {
            #[allow(unknown_lints, clippy::manual_inspect)]
            windows.0.borrow_mut().get_mut(&window_id).map(|window| {
              if let Some(i) = window.webviews.iter().position(|w| w.id == webview.id) {
                window.webviews.remove(i);
              }
              window
            });
          }
          WebviewMessage::SetBounds(bounds) => {
            let bounds: RectWrapper = bounds.into();
            let bounds = bounds.0;

            if let Some(b) = &mut *webview.bounds.lock().unwrap() {
              let scale_factor = window.scale_factor();
              let size = bounds.size.to_logical::<f32>(scale_factor);
              let position = bounds.position.to_logical::<f32>(scale_factor);
              let window_size = window.inner_size().to_logical::<f32>(scale_factor);
              b.width_rate = size.width / window_size.width;
              b.height_rate = size.height / window_size.height;
              b.x_rate = position.x / window_size.width;
              b.y_rate = position.y / window_size.height;
            }

            if let Err(e) = webview.set_bounds(bounds) {
              log::error!("failed to set webview size: {e}");
            }
          }
          WebviewMessage::SetSize(size) => match webview.bounds() {
            Ok(mut bounds) => {
              bounds.size = size;

              let scale_factor = window.scale_factor();
              let size = size.to_logical::<f32>(scale_factor);

              if let Some(b) = &mut *webview.bounds.lock().unwrap() {
                let window_size = window.inner_size().to_logical::<f32>(scale_factor);
                b.width_rate = size.width / window_size.width;
                b.height_rate = size.height / window_size.height;
              }

              if let Err(e) = webview.set_bounds(bounds) {
                log::error!("failed to set webview size: {e}");
              }
            }
            Err(e) => {
              log::error!("failed to get webview bounds: {e}");
            }
          },
          WebviewMessage::SetPosition(position) => match webview.bounds() {
            Ok(mut bounds) => {
              bounds.position = position;

              let scale_factor = window.scale_factor();
              let position = position.to_logical::<f32>(scale_factor);

              if let Some(b) = &mut *webview.bounds.lock().unwrap() {
                let window_size = window.inner_size().to_logical::<f32>(scale_factor);
                b.x_rate = position.x / window_size.width;
                b.y_rate = position.y / window_size.height;
              }

              if let Err(e) = webview.set_bounds(bounds) {
                log::error!("failed to set webview position: {e}");
              }
            }
            Err(e) => {
              log::error!("failed to get webview bounds: {e}");
            }
          },
          WebviewMessage::SetZoom(scale_factor) => {
            if let Err(e) = webview.zoom(scale_factor) {
              log::error!("failed to set webview zoom: {e}");
            }
          }
          WebviewMessage::SetBackgroundColor(color) => {
            if let Err(e) =
              webview.set_background_color(color.map(Into::into).unwrap_or((255, 255, 255, 255)))
            {
              log::error!("failed to set webview background color: {e}");
            }
          }
          WebviewMessage::ClearAllBrowsingData => {
            if let Err(e) = webview.clear_all_browsing_data() {
              log::error!("failed to clear webview browsing data: {e}");
            }
          }
          // Getters
          WebviewMessage::Url(tx) => {
            tx.send(
              webview
                .url()
                .map(|u| u.parse().expect("invalid webview URL"))
                .map_err(|_| Error::FailedToSendMessage),
            )
            .unwrap();
          }

          WebviewMessage::Cookies(tx) => {
            tx.send(webview.cookies().map_err(|_| Error::FailedToSendMessage))
              .unwrap();
          }

          WebviewMessage::SetCookie(cookie) => {
            if let Err(e) = webview.set_cookie(&cookie) {
              log::error!("failed to set webview cookie: {e}");
            }
          }

          WebviewMessage::DeleteCookie(cookie) => {
            if let Err(e) = webview.delete_cookie(&cookie) {
              log::error!("failed to delete webview cookie: {e}");
            }
          }

          WebviewMessage::CookiesForUrl(url, tx) => {
            let webview_cookies = webview
              .cookies_for_url(url.as_str())
              .map_err(|_| Error::FailedToSendMessage);
            tx.send(webview_cookies).unwrap();
          }

          WebviewMessage::Bounds(tx) => {
            tx.send(
              webview
                .bounds()
                .map(|bounds| tauri_runtime::dpi::Rect {
                  size: bounds.size,
                  position: bounds.position,
                })
                .map_err(|_| Error::FailedToSendMessage),
            )
            .unwrap();
          }
          WebviewMessage::Position(tx) => {
            tx.send(
              webview
                .bounds()
                .map(|bounds| bounds.position.to_physical(window.scale_factor()))
                .map_err(|_| Error::FailedToSendMessage),
            )
            .unwrap();
          }
          WebviewMessage::Size(tx) => {
            tx.send(
              webview
                .bounds()
                .map(|bounds| bounds.size.to_physical(window.scale_factor()))
                .map_err(|_| Error::FailedToSendMessage),
            )
            .unwrap();
          }
          WebviewMessage::SetFocus => {
            if let Err(e) = webview.focus() {
              log::error!("failed to focus webview: {e}");
            }
          }
          WebviewMessage::SetAutoResize(auto_resize) => match webview.bounds() {
            Ok(bounds) => {
              let scale_factor = window.scale_factor();
              let window_size = window.inner_size().to_logical::<f32>(scale_factor);
              *webview.bounds.lock().unwrap() = if auto_resize {
                let size = bounds.size.to_logical::<f32>(scale_factor);
                let position = bounds.position.to_logical::<f32>(scale_factor);
                Some(WebviewBounds {
                  x_rate: position.x / window_size.width,
                  y_rate: position.y / window_size.height,
                  width_rate: size.width / window_size.width,
                  height_rate: size.height / window_size.height,
                })
              } else {
                None
              };
            }
            Err(e) => {
              log::error!("failed to get webview bounds: {e}");
            }
          },
          WebviewMessage::WithWebview(f) => {
            #[cfg(any(
              target_os = "linux",
              target_os = "dragonfly",
              target_os = "freebsd",
              target_os = "netbsd",
              target_os = "openbsd"
            ))]
            {
              // No platform webview handle to expose on Linux; consume the
              // closure so the arm is uniform across targets.
              let _ = f;
            }
            #[cfg(target_os = "macos")]
            {
              f(Webview);
            }
            #[cfg(target_os = "ios")]
            {
              f(Webview);
            }
            #[cfg(windows)]
            {
              f(Webview);
            }
            #[cfg(target_os = "android")]
            {
              f(webview.handle())
            }
          }
          #[cfg(any(debug_assertions, feature = "devtools"))]
          WebviewMessage::OpenDevTools => {
            webview.open_devtools();
          }
          #[cfg(any(debug_assertions, feature = "devtools"))]
          WebviewMessage::CloseDevTools => {
            webview.close_devtools();
          }
          #[cfg(any(debug_assertions, feature = "devtools"))]
          WebviewMessage::IsDevToolsOpen(tx) => {
            tx.send(webview.is_devtools_open()).unwrap();
          }
        }
      }
    }
    Message::CreateWebview(window_id, handler, sender) => {
      let window = windows
        .0
        .borrow()
        .get(&window_id)
        .map(|w| (w.inner.clone(), w.focused_webview.clone()));
      if let Some((Some(window), focused_webview)) = window {
        match handler(&window, CreateWebviewOptions { focused_webview }) {
          Ok(webview) => {
            if let Some(w) = windows.0.borrow_mut().get_mut(&window_id) {
              w.webviews.push(webview);
              w.has_children.store(true, Ordering::Relaxed);
            }
            // SAFETY: The caller calls blocking `rx.recv()` so the receiver will never be dropped before this
            sender.send(Ok(())).unwrap();
          }
          Err(e) => {
            // SAFETY: The caller calls blocking `rx.recv()` so the receiver will never be dropped before this
            sender.send(Err(e)).unwrap();
          }
        }
      }
    }
    Message::CreateWindow(window_id, handler, sender) => match handler(event_loop) {
      Ok(webview) => {
        windows.0.borrow_mut().insert(window_id, webview);
        // SAFETY: The caller calls blocking `rx.recv()` so the receiver will never be dropped before this
        sender.send(Ok(())).unwrap();
      }
      Err(e) => {
        // SAFETY: The caller calls blocking `rx.recv()` so the receiver will never be dropped before this
        sender.send(Err(e)).unwrap();
      }
    },
    Message::CreateRawWindow(window_id, handler, sender) => {
      let (label, builder) = handler();

      #[cfg(windows)]
      let background_color = builder.window.background_color;
      #[cfg(windows)]
      let is_window_transparent = builder.window.transparent;

      if let Ok(window) = builder.build(event_loop) {
        window_id_map.insert(window.id(), window_id);

        let window = Arc::new(window);

        #[cfg(windows)]
        let surface = if is_window_transparent {
          if let Ok(context) = softbuffer::Context::new(window.clone()) {
            if let Ok(mut surface) = softbuffer::Surface::new(&context, window.clone()) {
              window.draw_surface(&mut surface, background_color);
              Some(surface)
            } else {
              None
            }
          } else {
            None
          }
        } else {
          None
        };

        windows.0.borrow_mut().insert(
          window_id,
          WindowWrapper {
            label,
            has_children: AtomicBool::new(false),
            inner: Some(window.clone()),
            window_event_listeners: Default::default(),
            webviews: Vec::new(),
            #[cfg(windows)]
            background_color,
            #[cfg(windows)]
            is_window_transparent,
            #[cfg(windows)]
            surface,
            focused_webview: Default::default(),
          },
        );
        sender.send(Ok(Arc::downgrade(&window))).unwrap();
      } else {
        sender.send(Err(Error::CreateWindow)).unwrap();
      }
    }

    Message::ServoWake(_, _) => (),
    Message::UserEvent(_) => (),
    Message::EventLoopWindowTarget(message) => match message {
      EventLoopWindowTargetMessage::CursorPosition(sender) => {
        let pos = event_loop
          .cursor_position()
          .map_err(|_| Error::FailedToSendMessage);
        sender.send(pos).unwrap();
      }
      EventLoopWindowTargetMessage::SetTheme(theme) => {
        event_loop.set_theme(to_tao_theme(theme));
      }
      EventLoopWindowTargetMessage::SetDeviceEventFilter(filter) => {
        event_loop.set_device_event_filter(DeviceEventFilterWrapper::from(filter).0);
      }
    },
  }
}

fn handle_event_loop<T: UserEvent>(
  event: Event<'_, Message<T>>,
  event_loop: &EventLoopWindowTarget<Message<T>>,
  control_flow: &mut ControlFlow,
  callback: &mut (dyn FnMut(RunEvent<T>) + 'static),
  window_id_map: &WindowIdStore,
  windows: &WindowsStore,
  #[cfg(feature = "tracing")] active_tracing_spans: &ActiveTraceSpanStore,
) {
  if *control_flow != ControlFlow::Exit {
    *control_flow = ControlFlow::Wait;
  }

  match event {
    Event::NewEvents(StartCause::Init) => {
      callback(RunEvent::Ready);
    }

    Event::NewEvents(StartCause::Poll) => {
      callback(RunEvent::Resumed);
    }

    Event::MainEventsCleared => {
      if windows.0.borrow().values().any(|window| {
        window
          .webviews
          .iter()
          .any(|webview| webview.servo().is_animating())
      }) {
        *control_flow = ControlFlow::Poll;
      }
      callback(RunEvent::MainEventsCleared);
    }

    Event::LoopDestroyed => {
      callback(RunEvent::Exit);
    }

    Event::RedrawRequested(id) => {
      if let Some(window_id) = window_id_map.get(&id) {
        if let Some(window) = windows.0.borrow().get(&window_id) {
          for webview in &window.webviews {
            webview.servo().paint();
          }
        }
      }
    }

    #[cfg(feature = "tracing")]
    Event::RedrawEventsCleared => {
      active_tracing_spans.remove_window_draw();
    }

    Event::UserEvent(Message::Webview(
      window_id,
      webview_id,
      WebviewMessage::WebviewEvent(event),
    )) => {
      let windows_ref = windows.0.borrow();
      if let Some(window) = windows_ref.get(&window_id) {
        if let Some(webview) = window.webviews.iter().find(|w| w.id == webview_id) {
          let label = webview.label.clone();
          let webview_event_listeners = webview.webview_event_listeners.clone();

          drop(windows_ref);

          callback(RunEvent::WebviewEvent {
            label,
            event: event.clone(),
          });
          let listeners = webview_event_listeners.lock().unwrap();
          let handlers = listeners.values();
          for handler in handlers {
            handler(&event);
          }
        }
      }
    }

    Event::UserEvent(Message::Webview(
      window_id,
      _webview_id,
      WebviewMessage::SynthesizedWindowEvent(event),
    )) => {
      if let Some(event) = WindowEventWrapper::from(event).0 {
        let windows_ref = windows.0.borrow();
        let window = windows_ref.get(&window_id);
        if let Some(window) = window {
          let label = window.label.clone();
          let window_event_listeners = window.window_event_listeners.clone();

          drop(windows_ref);

          callback(RunEvent::WindowEvent {
            label,
            event: event.clone(),
          });

          let listeners = window_event_listeners.lock().unwrap();
          let handlers = listeners.values();
          for handler in handlers {
            handler(&event);
          }
        }
      }
    }

    Event::WindowEvent {
      event, window_id, ..
    } => {
      if let Some(window_id) = window_id_map.get(&window_id) {
        if let Some(window) = windows.0.borrow().get(&window_id) {
          for webview in &window.webviews {
            webview.servo().handle_window_event(&event);
          }
        }

        {
          let windows_ref = windows.0.borrow();
          if let Some(window) = windows_ref.get(&window_id) {
            if let Some(event) = WindowEventWrapper::parse(window, &event).0 {
              let label = window.label.clone();
              let window_event_listeners = window.window_event_listeners.clone();

              drop(windows_ref);

              callback(RunEvent::WindowEvent {
                label,
                event: event.clone(),
              });
              let listeners = window_event_listeners.lock().unwrap();
              let handlers = listeners.values();
              for handler in handlers {
                handler(&event);
              }
            }
          }
        }

        match event {
          #[cfg(windows)]
          TaoWindowEvent::ThemeChanged(theme) => {
            if let Some(window) = windows.0.borrow().get(&window_id) {
              for webview in &window.webviews {
                if let Err(e) = webview.set_theme(map_theme(&theme)) {
                  log::error!("failed to set theme: {e}");
                }
              }
            }
          }
          TaoWindowEvent::CloseRequested => {
            on_close_requested(callback, window_id, windows);
          }
          TaoWindowEvent::Destroyed => {
            let removed = windows.0.borrow_mut().remove(&window_id).is_some();
            if removed {
              let is_empty = windows.0.borrow().is_empty();
              if is_empty {
                let (tx, rx) = channel();
                callback(RunEvent::ExitRequested { code: None, tx });

                let recv = rx.try_recv();
                let should_prevent = matches!(recv, Ok(ExitRequestedEventAction::Prevent));

                if !should_prevent {
                  *control_flow = ControlFlow::Exit;
                }
              }
            }
          }
          TaoWindowEvent::Resized(size) => {
            // keep the transparent-window backdrop surface sized with the
            // window, as the wry runtime does
            #[cfg(windows)]
            {
              let mut windows_ref = windows.0.borrow_mut();
              if let Some(window) = windows_ref.get_mut(&window_id) {
                if window.is_window_transparent {
                  let background_color = window.background_color;
                  if let Some(surface) = &mut window.surface {
                    if let Some(window) = &window.inner {
                      window.draw_surface(surface, background_color);
                    }
                  }
                }
              }
            }
            if let Some((Some(window), webviews)) = windows
              .0
              .borrow()
              .get(&window_id)
              .map(|w| (w.inner.clone(), w.webviews.clone()))
            {
              let size = size.to_logical::<f32>(window.scale_factor());
              for webview in webviews {
                if let Some(b) = &*webview.bounds.lock().unwrap() {
                  if let Err(e) = webview.set_bounds(Rect {
                    position: LogicalPosition::new(size.width * b.x_rate, size.height * b.y_rate)
                      .into(),
                    size: LogicalSize::new(size.width * b.width_rate, size.height * b.height_rate)
                      .into(),
                  }) {
                    log::error!("failed to autoresize webview: {e}");
                  }
                }
              }
            }
          }
          _ => {}
        }
      }
    }
    Event::UserEvent(message) => match message {
      Message::ServoWake(window_id, webview_id) => {
        if let Some(window) = windows.0.borrow().get(&window_id) {
          if let Some(webview) = window
            .webviews
            .iter()
            .find(|webview| webview.id == webview_id)
          {
            webview.servo().handle_user_event();
          }
        }
      }
      Message::RequestExit(code) => {
        let (tx, rx) = channel();
        callback(RunEvent::ExitRequested {
          code: Some(code),
          tx,
        });

        let recv = rx.try_recv();
        let should_prevent = matches!(recv, Ok(ExitRequestedEventAction::Prevent));

        if !should_prevent {
          *control_flow = ControlFlow::Exit;
        }
      }
      Message::Window(id, WindowMessage::Close) => {
        on_close_requested(callback, id, windows);
      }
      Message::Window(id, WindowMessage::Destroy) => {
        on_window_close(id, windows);
      }
      Message::UserEvent(t) => callback(RunEvent::UserEvent(t)),
      message => {
        handle_user_message(
          event_loop,
          message,
          UserMessageContext {
            window_id_map,
            windows,
          },
        );
      }
    },
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    Event::Opened { urls } => {
      callback(RunEvent::Opened { urls });
    }
    #[cfg(target_os = "macos")]
    Event::Reopen {
      has_visible_windows,
      ..
    } => callback(RunEvent::Reopen {
      has_visible_windows,
    }),
    #[cfg(target_os = "ios")]
    Event::SceneRequested { scene, options } => {
      callback(RunEvent::SceneRequested { scene, options });
    }
    _ => (),
  }
}

fn on_close_requested<'a, T: UserEvent>(
  callback: &'a mut (dyn FnMut(RunEvent<T>) + 'static),
  window_id: WindowId,
  windows: &WindowsStore,
) {
  let (tx, rx) = channel();
  let windows_ref = windows.0.borrow();
  if let Some(w) = windows_ref.get(&window_id) {
    let label = w.label.clone();
    let window_event_listeners = w.window_event_listeners.clone();

    drop(windows_ref);

    let listeners = window_event_listeners.lock().unwrap();
    let handlers = listeners.values();
    for handler in handlers {
      handler(&WindowEvent::CloseRequested {
        signal_tx: tx.clone(),
      });
    }
    callback(RunEvent::WindowEvent {
      label,
      event: WindowEvent::CloseRequested { signal_tx: tx },
    });
    if let Ok(true) = rx.try_recv() {
    } else {
      on_window_close(window_id, windows);
    }
  }
}

fn on_window_close(window_id: WindowId, windows: &WindowsStore) {
  if let Some(window_wrapper) = windows.0.borrow_mut().get_mut(&window_id) {
    window_wrapper.inner = None;
    #[cfg(windows)]
    window_wrapper.surface.take();
  }
}

fn parse_proxy_url(url: &Url) -> Result<ProxyConfig> {
  let host = url.host().map(|h| h.to_string()).unwrap_or_default();
  let port = url.port().unwrap_or(0);

  if url.scheme() == "http" {
    let config = ProxyConfig::Http(ProxyEndpoint { host, port });

    Ok(config)
  } else if url.scheme() == "socks5" {
    let config = ProxyConfig::Socks5(ProxyEndpoint { host, port });

    Ok(config)
  } else {
    Err(Error::InvalidProxyUrl)
  }
}

fn create_window<T: UserEvent, F: Fn(RawWindow) + Send + 'static>(
  window_id: WindowId,
  webview_id: u32,
  event_loop: &EventLoopWindowTarget<Message<T>>,
  context: &Context<T>,
  pending: PendingWindow<T, Servo<T>>,
  after_window_creation: Option<F>,
) -> Result<WindowWrapper> {
  #[allow(unused_mut)]
  let PendingWindow {
    mut window_builder,
    label,
    webview,
  } = pending;

  #[cfg(feature = "tracing")]
  let _webview_create_span = tracing::debug_span!("servo::webview::create").entered();
  #[cfg(feature = "tracing")]
  let window_draw_span = tracing::debug_span!("servo::window::draw").entered();
  #[cfg(feature = "tracing")]
  let window_create_span =
    tracing::debug_span!(parent: &window_draw_span, "servo::window::create").entered();

  let window_event_listeners = WindowEventListeners::default();

  #[cfg(windows)]
  let background_color = window_builder.inner.window.background_color;
  #[cfg(windows)]
  let is_window_transparent = window_builder.inner.window.transparent;

  #[cfg(target_os = "macos")]
  {
    if window_builder.tabbing_identifier.is_none()
      || window_builder.inner.window.transparent
      || !window_builder.inner.window.decorations
    {
      window_builder.inner = window_builder.inner.with_automatic_window_tabbing(false);
    }
  }

  #[cfg(desktop)]
  if window_builder.prevent_overflow.is_some() || window_builder.center {
    let monitor = if let Some(window_position) = &window_builder.inner.window.position {
      find_monitor_for_position(event_loop.available_monitors(), *window_position)
    } else {
      event_loop.primary_monitor()
    };
    if let Some(monitor) = monitor {
      let scale_factor = monitor.scale_factor();
      let desired_size = window_builder
        .inner
        .window
        .inner_size
        .unwrap_or_else(|| PhysicalSize::new(800, 600).into());
      let mut inner_size = window_builder
        .inner
        .window
        .inner_size_constraints
        .clamp(desired_size, scale_factor)
        .to_physical::<u32>(scale_factor);
      let mut window_size = inner_size;
      #[allow(unused_mut)]
      // Left and right window shadow counts as part of the window on Windows
      // We need to include it when calculating positions, but not size
      let mut shadow_width = 0;
      #[cfg(windows)]
      if window_builder.inner.window.decorations {
        use windows::Win32::UI::WindowsAndMessaging::{AdjustWindowRect, WS_OVERLAPPEDWINDOW};
        let mut rect = windows::Win32::Foundation::RECT::default();
        let result = unsafe { AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false) };
        if result.is_ok() {
          shadow_width = (rect.right - rect.left) as u32;
          // rect.bottom is made out of shadow, and we don't care about it
          window_size.height += -rect.top as u32;
        }
      }

      if let Some(margin) = window_builder.prevent_overflow {
        let work_area = monitor.work_area();
        let margin = margin.to_physical::<u32>(scale_factor);
        let constraint = PhysicalSize::new(
          work_area.size.width - margin.width,
          work_area.size.height - margin.height,
        );
        if window_size.width > constraint.width || window_size.height > constraint.height {
          if window_size.width > constraint.width {
            inner_size.width = inner_size
              .width
              .saturating_sub(window_size.width - constraint.width);
            window_size.width = constraint.width;
          }
          if window_size.height > constraint.height {
            inner_size.height = inner_size
              .height
              .saturating_sub(window_size.height - constraint.height);
            window_size.height = constraint.height;
          }
          window_builder.inner.window.inner_size = Some(inner_size.into());
        }
      }

      if window_builder.center {
        window_size.width += shadow_width;
        let position = window::calculate_window_center_position(window_size, monitor);
        let logical_position = position.to_logical::<f64>(scale_factor);
        window_builder = window_builder.position(logical_position.x, logical_position.y);
      }
    }
  };

  #[cfg(any(target_os = "macos", target_os = "linux"))]
  let (initial_position, is_fullscreen) = (
    window_builder.inner.window.position,
    window_builder.inner.window.fullscreen.is_some(),
  );

  // If fullscreen is requested with an explicit position, resolve the target
  // monitor up front so the window is created fullscreen on that display.
  #[cfg(any(target_os = "macos", target_os = "linux"))]
  if let (true, Some(position)) = (is_fullscreen, initial_position) {
    if let Some(target_monitor) =
      find_monitor_for_position(event_loop.available_monitors(), position)
    {
      window_builder.inner.window.fullscreen = Some(Fullscreen::Borderless(Some(target_monitor)));
    }
  }

  let window = window_builder
    .inner
    .build(event_loop)
    .inspect_err(|e| log::error!("Error creating window: {e:?}"))
    .map_err(|_| Error::CreateWindow)?;

  // On macOS, `with_position` uses the content origin; the title bar is added
  // above it. `set_outer_position` is needed for precise window placement.
  #[cfg(target_os = "macos")]
  if !is_fullscreen {
    if let Some(position) = initial_position {
      window.set_outer_position(position);
    }
  }

  #[cfg(feature = "tracing")]
  {
    drop(window_create_span);

    context
      .main_thread
      .active_tracing_spans
      .0
      .borrow_mut()
      .push(ActiveTracingSpan::WindowDraw {
        id: window.id(),
        span: window_draw_span,
      });
  }

  context.window_id_map.insert(window.id(), window_id);

  if let Some(handler) = after_window_creation {
    let raw = RawWindow {
      #[cfg(windows)]
      hwnd: window.hwnd(),
      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      gtk_window: window.gtk_window(),
      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      default_vbox: window.default_vbox(),
      _marker: &std::marker::PhantomData,
    };
    handler(raw);
  }

  let mut webviews = Vec::new();

  let focused_webview = Arc::new(Mutex::new(FocusState::default()));

  #[cfg(feature = "unstable")]
  let has_children = webview.is_some();
  #[cfg(not(feature = "unstable"))]
  let has_children = false;

  if let Some(webview) = webview {
    webviews.push(create_webview(
      #[cfg(feature = "unstable")]
      WebviewKind::WindowChild,
      #[cfg(not(feature = "unstable"))]
      WebviewKind::WindowContent,
      &window,
      Arc::new(Mutex::new(window_id)),
      webview_id,
      context,
      webview,
      focused_webview.clone(),
    )?);
  }

  let window = Arc::new(window);

  #[cfg(windows)]
  let surface = if is_window_transparent {
    if let Ok(context) = softbuffer::Context::new(window.clone()) {
      if let Ok(mut surface) = softbuffer::Surface::new(&context, window.clone()) {
        window.draw_surface(&mut surface, background_color);
        Some(surface)
      } else {
        None
      }
    } else {
      None
    }
  } else {
    None
  };

  Ok(WindowWrapper {
    label,
    has_children: AtomicBool::new(has_children),
    inner: Some(window),
    webviews,
    window_event_listeners,
    #[cfg(windows)]
    background_color,
    #[cfg(windows)]
    is_window_transparent,
    #[cfg(windows)]
    surface,
    focused_webview,
  })
}

/// the kind of the webview
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum WebviewKind {
  /// webview is the entire window content
  WindowContent,
  /// webview is a child of the window, which can contain other webviews too
  WindowChild,
}

#[derive(Debug, Clone)]
struct WebviewBounds {
  x_rate: f32,
  y_rate: f32,
  width_rate: f32,
  height_rate: f32,
}

fn create_webview<T: UserEvent>(
  _kind: WebviewKind,
  window: &Window,
  window_id: Arc<Mutex<WindowId>>,
  id: WebviewId,
  context: &Context<T>,
  pending: PendingWebview<T, Servo<T>>,
  #[allow(unused_variables)] focused_webview: Arc<Mutex<FocusState>>,
) -> Result<WebviewWrapper> {
  #[allow(unused_mut)]
  let PendingWebview {
    webview_attributes,
    uri_scheme_protocols,
    label,
    ipc_handler,
    url,
    ..
  } = pending;

  let mut webview_builder = WebViewBuilder::new()
    .with_id(&label)
    .with_transparent(webview_attributes.transparent);

  if url != "about:blank" {
    webview_builder = webview_builder.with_url(&url);
  }

  if webview_attributes.javascript_disabled {
    webview_builder = webview_builder.with_javascript_disabled();
  }

  if let Some(color) = webview_attributes.background_color {
    webview_builder = webview_builder.with_background_color(color.into());
  }

  if let Some(navigation_handler) = pending.navigation_handler {
    webview_builder = webview_builder.with_navigation_handler(move |url| {
      url
        .parse()
        .map(|url| navigation_handler(&url))
        .unwrap_or(true)
    });
  }

  if let Some(new_window_handler) = pending.new_window_handler {
    let _ = new_window_handler;
  }

  if let Some(document_title_changed_handler) = pending.document_title_changed_handler {
    webview_builder =
      webview_builder.with_document_title_changed_handler(document_title_changed_handler)
  }

  let webview_bounds = if let Some(bounds) = webview_attributes.bounds {
    let bounds: RectWrapper = bounds.into();
    let bounds = bounds.0;

    let scale_factor = window.scale_factor();
    let position = bounds.position.to_logical::<f32>(scale_factor);
    let size = bounds.size.to_logical::<f32>(scale_factor);

    webview_builder = webview_builder.with_bounds(bounds);

    let window_size = window.inner_size().to_logical::<f32>(scale_factor);

    if webview_attributes.auto_resize {
      Some(WebviewBounds {
        x_rate: position.x / window_size.width,
        y_rate: position.y / window_size.height,
        width_rate: size.width / window_size.width,
        height_rate: size.height / window_size.height,
      })
    } else {
      None
    }
  } else {
    webview_builder = webview_builder.with_bounds(Rect {
      position: LogicalPosition::new(0, 0).into(),
      size: window.inner_size().into(),
    });
    Some(WebviewBounds {
      x_rate: 0.,
      y_rate: 0.,
      width_rate: 1.,
      height_rate: 1.,
    })
  };

  if let Some(download_handler) = pending.download_handler {
    let download_handler_ = download_handler.clone();
    webview_builder = webview_builder.with_download_started_handler(move |url, path| {
      if let Ok(url) = url.parse() {
        download_handler_(DownloadEvent::Requested {
          url,
          destination: path,
        })
      } else {
        false
      }
    });
    webview_builder = webview_builder.with_download_completed_handler(move |url, path, success| {
      if let Ok(url) = url.parse() {
        download_handler(DownloadEvent::Finished { url, path, success });
      }
    });
  }

  if let Some(page_load_handler) = pending.on_page_load_handler {
    webview_builder = webview_builder.with_on_page_load_handler(move |event, url| {
      if let Ok(url) = url.parse() {
        page_load_handler(
          url,
          match event {
            PageLoadEvent::Started => tauri_runtime::webview::PageLoadEvent::Started,
            PageLoadEvent::Finished => tauri_runtime::webview::PageLoadEvent::Finished,
          },
        )
      };
    });
  }

  if let Some(user_agent) = webview_attributes.user_agent {
    webview_builder = webview_builder.with_user_agent(&user_agent);
  }

  if let Some(proxy_url) = webview_attributes.proxy_url {
    let config = parse_proxy_url(&proxy_url)?;

    webview_builder = webview_builder.with_proxy_config(config);
  }

  webview_builder = webview_builder.with_ipc_handler(create_ipc_handler(
    window_id.clone(),
    id,
    context.clone(),
    label.clone(),
    ipc_handler,
  ));

  for script in webview_attributes.initialization_scripts {
    webview_builder = webview_builder
      .with_initialization_script_for_main_only(script.script, script.for_main_frame_only);
  }

  for (scheme, protocol) in uri_scheme_protocols {
    webview_builder = webview_builder.with_asynchronous_custom_protocol(
      scheme,
      move |webview_id, request, responder| {
        protocol(
          webview_id,
          request,
          Box::new(move |response| responder.respond(response)),
        )
      },
    );
  }

  #[cfg(target_os = "android")]
  {
    if let Some(on_webview_created) = pending.on_webview_created {
      webview_builder = webview_builder.on_webview_created(move |ctx| {
        on_webview_created(tauri_runtime::webview::CreationContext {
          env: ctx.env,
          activity: ctx.activity,
          webview: ctx.webview,
        })
      });
    }
  }

  let webview = {
    let proxy = context.proxy.clone();
    let servo_window_id = *window_id.lock().unwrap();
    webview_builder.build_servo_as_child(window, move || {
      let _ = proxy.send_event(Message::ServoWake(servo_window_id, id));
    })
  };

  let webview = webview.map_err(|e| Error::CreateWebview(Box::new(e)))?;

  Ok(WebviewWrapper {
    label,
    id,
    inner: Rc::new(webview),
    webview_event_listeners: Default::default(),
    bounds: Arc::new(Mutex::new(webview_bounds)),
  })
}

/// Create a Servo ipc handler from a tauri ipc handler.
fn create_ipc_handler<T: UserEvent>(
  window_id: Arc<Mutex<WindowId>>,
  webview_id: WebviewId,
  context: Context<T>,
  label: String,
  ipc_handler: Option<WebviewIpcHandler<T, Servo<T>>>,
) -> Box<IpcHandler> {
  Box::new(move |request| {
    if let Some(handler) = &ipc_handler {
      handler(
        DetachedWebview {
          label: label.clone(),
          dispatcher: ServoWebviewDispatcher {
            window_id: window_id.clone(),
            webview_id,
            context: context.clone(),
          },
        },
        request,
      );
    }
  })
}

#[allow(unused_variables)]
fn inner_size(
  window: &Window,
  webviews: &[WebviewWrapper],
  has_children: bool,
) -> PhysicalSize<u32> {
  window.inner_size()
}

fn to_tao_theme(theme: Option<Theme>) -> Option<TaoTheme> {
  match theme {
    Some(Theme::Light) => Some(TaoTheme::Light),
    Some(Theme::Dark) => Some(TaoTheme::Dark),
    _ => None,
  }
}
