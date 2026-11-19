//! Diagnostic-only hidden full-view candidate. No retries or RTSS configuration access.
use std::{fs::OpenOptions,io::Write,path::Path,sync::{Mutex,OnceLock,atomic::{AtomicBool,Ordering}},time::{Instant,Duration}};
use tauri::{Manager,WebviewWindowBuilder,WindowBuilder};
struct Probe {start:Instant,file:Mutex<std::fs::File>,hidden:bool,ready:AtomicBool,controller:AtomicBool,shown:AtomicBool,failed:AtomicBool,closed:AtomicBool}
static PROBE:OnceLock<Probe>=OnceLock::new();
pub fn begin()->std::io::Result<()> {
 let Ok(mode)=std::env::var("SUONELO_PRELOAD_MODE") else{return Ok(())};
 if !["normal","hidden"].contains(&mode.as_str()){return Err(std::io::Error::other("unknown prototype mode"))}
 let exe=std::env::current_exe()?;let root=exe.parent().unwrap();
 if !root.components().any(|p|p.as_os_str()=="round8-preload-suite"){return Err(std::io::Error::other("prototype requires isolated suite root"))}
 let file=OpenOptions::new().create_new(true).write(true).open(root.join("startup.jsonl"))?;
 PROBE.set(Probe{start:Instant::now(),file:Mutex::new(file),hidden:mode=="hidden",ready:AtomicBool::new(false),controller:AtomicBool::new(false),shown:AtomicBool::new(false),failed:AtomicBool::new(false),closed:AtomicBool::new(false)}).map_err(|_|std::io::Error::other("duplicate probe"))?;
 mark("host_started",serde_json::json!({"mode":mode,"attempt":1,"recovery":false}));Ok(())
}
pub fn mark(stage:&str,detail:serde_json::Value){if let Some(p)=PROBE.get(){let mut f=p.file.lock().unwrap();let _=writeln!(f,"{}",serde_json::json!({"stage":stage,"ms":p.start.elapsed().as_secs_f64()*1000.0,"detail":detail}));let _=f.flush();}}
pub fn enabled()->bool{PROBE.get().is_some()}
fn fail(handle:&tauri::AppHandle,reason:String){if let Some(p)=PROBE.get(){if p.closed.load(Ordering::SeqCst)||p.failed.swap(true,Ordering::SeqCst){return}mark("native_error",serde_json::json!({"reason":reason}));if let Some(main)=handle.get_webview_window("main"){let _=main.hide();}let error_window=handle.get_window("startup").or_else(||WindowBuilder::new(handle,"startup").title("suonelo: Startup failed - Close to exit").inner_size(540.0,150.0).build().ok());if let Some(w)=error_window{let _=w.set_title("suonelo: Startup failed - Close to exit (no retry)");}}}
fn reveal(handle:&tauri::AppHandle){if let Some(p)=PROBE.get(){if p.ready.load(Ordering::SeqCst)&&p.controller.load(Ordering::SeqCst)&&!p.failed.load(Ordering::SeqCst)&&!p.closed.load(Ordering::SeqCst)&&!p.shown.swap(true,Ordering::SeqCst){mark("frontend_ready",serde_json::json!({"bootstrap_and_handlers":true}));if p.hidden {if let Some(w)=handle.get_webview_window("main"){let _=w.show();let _=w.set_focus();}}mark("main_window_shown",serde_json::json!({"hidden_candidate":p.hidden}));if let Some(w)=handle.get_window("startup"){let _=w.destroy();}}}}
pub fn frontend_ready(handle:&tauri::AppHandle){if let Some(p)=PROBE.get(){if !p.closed.load(Ordering::SeqCst){p.ready.store(true,Ordering::SeqCst);reveal(handle);}}}
pub fn native_close(window:&tauri::Window)->Option<bool>{if let Some(p)=PROBE.get(){if window.label()=="startup"||!p.shown.load(Ordering::SeqCst)||p.failed.load(Ordering::SeqCst){
 let state=window.state::<std::sync::Arc<crate::app::App>>();
 let pending=state.state().queue.pending();
 if pending {unsafe {use windows::{Win32::UI::WindowsAndMessaging::{MessageBoxW,MB_OKCANCEL,MB_ICONWARNING,IDOK},core::w};
 if MessageBoxW(None,w!("Active or queued work will stop. Finalized files will be kept. Close this test session?"),w!("suonelo"),MB_OKCANCEL|MB_ICONWARNING)!=IDOK {return Some(false)} }}
 p.closed.store(true,Ordering::SeqCst);mark("shutdown_started",serde_json::json!({"path":"native-pre-ready-or-failed-close","pending_work_confirmed":pending}));return Some(true)}}None}
pub fn build(app:&tauri::App,config:&tauri::utils::config::WindowConfig,root:&Path)->tauri::Result<()> {
 if !enabled(){WebviewWindowBuilder::from_config(app,config)?.data_directory(root.join("data/webview")).build()?;return Ok(())}
 let p=PROBE.get().unwrap();
 WindowBuilder::new(app,"startup").title("suonelo: Initializing WebView2 - Close to cancel").inner_size(540.0,150.0).build()?;
 let handle=app.handle().clone();let deadline=handle.clone();std::thread::spawn(move||{std::thread::sleep(Duration::from_secs(30));let Some(p)=PROBE.get() else{return};if !p.shown.load(Ordering::SeqCst)&&!p.closed.load(Ordering::SeqCst){let h=deadline.clone();let _=deadline.run_on_main_thread(move||fail(&h,"30-second startup deadline".into()));}});
 mark("environment_start",serde_json::json!({"final_profile":true,"candidate":"hidden-full-view"}));
 if std::env::var("SUONELO_PRELOAD_FAULT").as_deref()==Ok("before-build"){fail(&handle,"injected before-controller failure".into());return Ok(())}
 let w=WebviewWindowBuilder::from_config(app,config)?.visible(!p.hidden).data_directory(root.join("data/webview"))
 .initialization_script("window.__SUONELO_PRELOAD_DIAGNOSTIC__=true;")
 .on_page_load(|_,payload|mark("document_load_event",serde_json::json!({"event":format!("{:?}",payload.event())})))
 .build()?;
 if !p.hidden {mark("main_visible_at_creation",serde_json::json!({}));}
 w.with_webview(move|view|{
  mark("environment_ready",serde_json::json!({"observation":"environment exposed by supported Tauri platform API after build"}));
  unsafe {
   let result=(||->windows::core::Result<()> {let core=view.controller().CoreWebView2()?;let mut pid=0;core.BrowserProcessId(&mut pid)?;
    let h=handle.clone();let handler=webview2_com::ProcessFailedEventHandler::create(Box::new(move|_,args|{let kind=args.and_then(|a|{let mut kind=webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_PROCESS_FAILED_KIND(0);a.ProcessFailedKind(&mut kind).ok().map(|_|kind.0)});mark("browser_process_failed",serde_json::json!({"kind":kind}));fail(&h,"WebView2 ProcessFailed; no recovery".into());Ok(())}));let mut token=0;core.add_ProcessFailed(&handler,&mut token)?;
    mark("controller_ready",serde_json::json!({"browser_pid":pid,"process_failed_listener":true}));PROBE.get().unwrap().controller.store(true,Ordering::SeqCst);reveal(&handle);Ok(())})();
   if let Err(e)=result{fail(&handle,format!("native observer registration: {e}"));}
  }
 })?;Ok(())
}
