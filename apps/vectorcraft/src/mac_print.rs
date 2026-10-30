//! macOS: File → Print with System Dialog… (#4728). The print job's PDF (laid out by `file.print`
//! with marks, bleed and color management) goes through PDFKit's print operation
//! (<https://developer.apple.com/documentation/pdfkit/pdfdocument/printoperation(for:scalingmode:autorotate:)>),
//! scaled down to fit the paper, and AppKit shows the standard print panel for it, which has the
//! printer driver's own options (finishing, paper tray, color profile…) under "Printer Options".
//!
//! The panel is a sheet on the window ([`NSPrintOperation::runOperationModalForWindow_delegate_didRunSelector_contextInfo`]),
//! never `runOperation()`: that runs AppKit's modal event loop inside winit's event handler, where an
//! event winit held back meanwhile re-enters the handler and aborts the app (#867, as file dialogs
//! did). The sheet returns at once and the window keeps running.
//!
//! AppKit does not retain a sheet's delegate, and the operation needs its document, so one delegate
//! lives for the whole run and the last operation and document are kept until the next print.
//! Reading the window from its handle, sending the sheet's selector and the delegate class have no
//! safe API; this module allows `unsafe_code`.
#![allow(unsafe_code)]

use std::cell::{OnceCell, RefCell};
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSPrintInfo, NSPrintOperation, NSView};
use objc2_foundation::{NSData, NSObject, NSObjectProtocol, NSString};
use objc2_pdf_kit::{PDFDocument, PDFPrintScalingMode};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

define_class!(
    // SAFETY: NSObject has no extra subclassing requirements; the subclass has no ivars and
    // implements no Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "VectorcraftPrintDelegate"]
    struct PrintDelegate;

    // SAFETY: NSObjectProtocol has no additional requirements.
    unsafe impl NSObjectProtocol for PrintDelegate {}

    impl PrintDelegate {
        // SAFETY: AppKit calls the did-run selector of `runOperationModalForWindow:…` with the
        // operation, whether it printed, and the context pointer given there (null here).
        #[unsafe(method(printOperationDidRun:success:contextInfo:))]
        fn did_run(&self, _operation: &NSPrintOperation, success: bool, _context: *mut c_void) {
            // The panel's own progress and errors are AppKit's; nothing here waits for the answer.
            log::info!("system print panel closed (printed: {success})");
        }
    }
);

impl PrintDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // SAFETY: NSObject's designated initializer is called once on the allocated subclass.
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

/// What a running panel needs to stay alive.
struct Active {
    _document: Retained<PDFDocument>,
    _operation: Retained<NSPrintOperation>,
}

thread_local! {
    // Main thread only: every type in them is.
    static DELEGATE: OnceCell<Retained<PrintDelegate>> = const { OnceCell::new() };
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// Show the system's print panel for the PDF `pdf` (named `title` in the queue) as a sheet on
/// `window`. Returns once the sheet is shown, not once it closes.
pub fn show(window: &winit::window::Window, title: &str, pdf: &[u8]) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("the print panel can only be shown from the main thread")?;
    let handle = window.window_handle().map_err(|e| e.to_string())?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return Err("the window isn't an AppKit window".into()) };
    // SAFETY: the handle's view is the window's content view, which AppKit keeps alive while the
    // window is open, and winit hands out the handle only while it is.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let parent = view.window().ok_or("the window has no AppKit window yet")?;
    if parent.attachedSheet().is_some() {
        return Err("close the open sheet before printing".into());
    }
    let data = NSData::with_bytes(pdf);
    // SAFETY: `initWithData:` is called once on the freshly allocated document.
    let document = unsafe { PDFDocument::initWithData(PDFDocument::alloc(), &data) }.ok_or("PDFKit can't read the print job")?;
    let info = NSPrintInfo::sharedPrintInfo();
    // Down to fit: a page larger than the paper shrinks to it, one that fits keeps its size, so
    // the job's marks and bleed stay where `file.print` put them.
    // SAFETY: the print info is a valid NSPrintInfo and the call is made on the main thread.
    let operation =
        unsafe { document.printOperationForPrintInfo_scalingMode_autoRotate(Some(&info), PDFPrintScalingMode::PageScaleDownToFit, true, mtm) }
            .ok_or("PDFKit can't make a print operation for the job")?;
    operation.setJobTitle(Some(&NSString::from_str(title)));
    operation.setShowsPrintPanel(true);
    let delegate = DELEGATE.with(|d| d.get_or_init(|| PrintDelegate::new(mtm)).clone());
    let target: &AnyObject = &delegate;
    // The operation and its document stay until the next print replaces them.
    ACTIVE.with(|a| *a.borrow_mut() = Some(Active { _document: document, _operation: operation.clone() }));
    // SAFETY: the selector is the delegate's method above, with the signature AppKit calls; the
    // delegate lives for the whole run; the context pointer is null.
    unsafe {
        operation.runOperationModalForWindow_delegate_didRunSelector_contextInfo(
            &parent,
            Some(target),
            Some(sel!(printOperationDidRun:success:contextInfo:)),
            std::ptr::null_mut(),
        );
    }
    Ok(())
}
