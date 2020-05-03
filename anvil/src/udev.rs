use std::{
    cell::RefCell,
    collections::hash_map::{Entry, HashMap},
    io::Error as IoError,
    os::unix::io::{AsRawFd, RawFd},
    path::PathBuf,
    rc::Rc,
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};

use glium::Surface as GliumSurface;
use slog::Logger;

#[cfg(feature = "egl")]
use smithay::backend::egl::{display::EGLBufferReader, EGLGraphicsBackend};
use smithay::{
    backend::{
        drm::{
            atomic::AtomicDrmDevice,
            common::fallback::FallbackDevice,
            device_bind,
            egl::{EglDevice, EglSurface},
            gbm::{egl::Gbm as EglGbmBackend, GbmDevice, GbmSurface},
            legacy::LegacyDrmDevice,
            DevPath, Device, DeviceHandler, Surface,
        },
        graphics::{CursorBackend, SwapBuffersError},
        input::InputBackend,
        libinput::{libinput_bind, LibinputInputBackend, LibinputSessionInterface},
        session::{
            auto::{auto_session_bind, AutoSession},
            notify_multiplexer, AsSessionObserver, Session, SessionNotifier, SessionObserver,
        },
        udev::{primary_gpu, UdevBackend, UdevEvent},
    },
    reexports::{
        calloop::{
            generic::Generic,
            timer::{Timer, TimerHandle},
            EventLoop, LoopHandle, Source,
        },
        drm::{self, control::{
            connector::{Info as ConnectorInfo, State as ConnectorState},
            crtc,
            encoder::Info as EncoderInfo,
        }},
        image::{ImageBuffer, Rgba},
        input::Libinput,
        nix::{fcntl::OFlag, sys::stat::dev_t},
        wayland_server::{
            protocol::{wl_output, wl_surface},
            Display,
        },
    },
    wayland::{
        compositor::CompositorToken,
        data_device::set_data_device_focus,
        output::{Mode, Output, PhysicalProperties},
        seat::{CursorImageStatus, Seat, XkbConfig},
        SERIAL_COUNTER as SCOUNTER,
    },
};

use crate::buffer_utils::BufferUtils;
use crate::glium_drawer::{GliumDrawer, schedule_initial_render};
use crate::input_handler::{AnvilInputHandler, InputInitData};
use crate::shell::{MyWindowMap, Roles};
use crate::state::AnvilState;

#[derive(Clone)]
pub struct SessionFd(RawFd);
impl AsRawFd for SessionFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

type RenderDevice = EglDevice<
    EglGbmBackend<FallbackDevice<AtomicDrmDevice<SessionFd>, LegacyDrmDevice<SessionFd>>>,
    GbmDevice<FallbackDevice<AtomicDrmDevice<SessionFd>, LegacyDrmDevice<SessionFd>>>,
>;
type RenderSurface =
    EglSurface<GbmSurface<FallbackDevice<AtomicDrmDevice<SessionFd>, LegacyDrmDevice<SessionFd>>>>;

pub fn run_udev(
    display: Rc<RefCell<Display>>,
    event_loop: &mut EventLoop<AnvilState>,
    log: Logger,
) -> Result<(), ()> {
    let name = display
        .borrow_mut()
        .add_socket_auto()
        .unwrap()
        .into_string()
        .unwrap();
    info!(log, "Listening on wayland socket"; "name" => name.clone());
    ::std::env::set_var("WAYLAND_DISPLAY", name);

    #[cfg(feature = "egl")]
    let egl_buffer_reader = Rc::new(RefCell::new(None));

    #[cfg(feature = "egl")]
    let buffer_utils = BufferUtils::new(egl_buffer_reader.clone(), log.clone());
    #[cfg(not(feature = "egl"))]
    let buffer_utils = BufferUtils::new(log.clone());

    /*
     * Initialize the compositor
     */
    let mut state = AnvilState::init(display.clone(), event_loop.handle(), buffer_utils, log.clone());

    /*
     * Initialize session
     */
    let (session, mut notifier) = AutoSession::new(log.clone()).ok_or(())?;
    let (udev_observer, udev_notifier) = notify_multiplexer();
    let udev_session_id = notifier.register(udev_observer);

    let pointer_location = Rc::new(RefCell::new((0.0, 0.0)));
    let cursor_status = Arc::new(Mutex::new(CursorImageStatus::Default));

    /*
     * Initialize the udev backend
     */
    let seat = session.seat();

    let primary_gpu = primary_gpu(&seat).unwrap_or_default();

    let bytes = include_bytes!("../resources/cursor2.rgba");
    let udev_backend = UdevBackend::new(seat.clone(), log.clone()).map_err(|_| ())?;

    let mut udev_handler = UdevHandlerImpl {
        compositor_token: state.ctoken,
        #[cfg(feature = "egl")]
        egl_buffer_reader,
        session: session.clone(),
        backends: HashMap::new(),
        display: display.clone(),
        primary_gpu,
        window_map: state.window_map.clone(),
        pointer_location: pointer_location.clone(),
        pointer_image: ImageBuffer::from_raw(64, 64, bytes.to_vec()).unwrap(),
        cursor_status: cursor_status.clone(),
        dnd_icon: state.dnd_icon.clone(),
        loop_handle: event_loop.handle(),
        notifier: udev_notifier,
        logger: log.clone(),
    };

    /*
     * Initialize wayland input object
     */
    let (mut w_seat, _) = Seat::new(
        &mut display.borrow_mut(),
        session.seat(),
        state.ctoken,
        log.clone(),
    );

    let pointer = w_seat.add_pointer(state.ctoken.clone(), move |new_status| {
        *cursor_status.lock().unwrap() = new_status;
    });
    let keyboard = w_seat
        .add_keyboard(XkbConfig::default(), 200, 25, |seat, focus| {
            set_data_device_focus(seat, focus.and_then(|s| s.as_ref().client()))
        })
        .expect("Failed to initialize the keyboard");

    /*
     * Initialize a fake output (we render one screen to every device in this example)
     */
    let (output, _output_global) = Output::new(
        &mut display.borrow_mut(),
        "Drm".into(),
        PhysicalProperties {
            width: 0,
            height: 0,
            subpixel: wl_output::Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Generic DRM".into(),
        },
        log.clone(),
    );

    let (w, h) = (1920, 1080); // Hardcode full-hd res
    output.change_current_state(
        Some(Mode {
            width: w as i32,
            height: h as i32,
            refresh: 60_000,
        }),
        None,
        None,
    );
    output.set_preferred(Mode {
        width: w as i32,
        height: h as i32,
        refresh: 60_000,
    });

    /*
     * Initialize libinput backend
     */
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<AutoSession>>(session.clone().into());
    let libinput_session_id = notifier.register(libinput_context.observer());
    libinput_context.udev_assign_seat(&seat).unwrap();
    let mut libinput_backend = LibinputInputBackend::new(libinput_context, log.clone());
    libinput_backend.set_handler(AnvilInputHandler::new_with_session(
        log.clone(),
        InputInitData {
            pointer,
            keyboard,
            window_map: state.window_map.clone(),
            screen_size: (w, h),
            running: state.running.clone(),
            pointer_location,
        },
        session,
    ));

    /*
     * Bind all our objects that get driven by the event loop
     */
    let libinput_event_source = libinput_bind(libinput_backend, event_loop.handle())
        .map_err(|e| -> IoError { e.into() })
        .unwrap();
    let session_event_source = auto_session_bind(notifier, event_loop.handle())
        .map_err(|(e, _)| e)
        .unwrap();

    for (dev, path) in udev_backend.device_list() {
        udev_handler.device_added(dev, path.into())
    }

    let udev_event_source = event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, _state| match event {
            UdevEvent::Added { device_id, path } => udev_handler.device_added(device_id, path),
            UdevEvent::Changed { device_id } => udev_handler.device_changed(device_id),
            UdevEvent::Removed { device_id } => udev_handler.device_removed(device_id),
        })
        .map_err(|e| -> IoError { e.into() })
        .unwrap();

    /*
     * And run our loop
     */

    while state.running.load(Ordering::SeqCst) {
        if event_loop
            .dispatch(Some(Duration::from_millis(16)), &mut state)
            .is_err()
        {
            state.running.store(false, Ordering::SeqCst);
        } else {
            display.borrow_mut().flush_clients(&mut state);
            state.window_map.borrow_mut().refresh();
        }
    }

    // Cleanup stuff
    state.window_map.borrow_mut().clear();

    let mut notifier = session_event_source.unbind();
    notifier.unregister(libinput_session_id);
    notifier.unregister(udev_session_id);

    event_loop.handle().remove(libinput_event_source);
    event_loop.handle().remove(udev_event_source);

    Ok(())
}

struct BackendData<S: SessionNotifier> {
    id: S::Id,
    restart_id: S::Id,
    event_source: Source<Generic<RenderDevice>>,
    surfaces: Rc<RefCell<HashMap<crtc::Handle, Rc<GliumDrawer<RenderSurface>>>>>,
}

struct UdevHandlerImpl<S: SessionNotifier, Data: 'static> {
    compositor_token: CompositorToken<Roles>,
    #[cfg(feature = "egl")]
    egl_buffer_reader: Rc<RefCell<Option<EGLBufferReader>>>,
    session: AutoSession,
    backends: HashMap<dev_t, BackendData<S>>,
    display: Rc<RefCell<Display>>,
    primary_gpu: Option<PathBuf>,
    window_map: Rc<RefCell<MyWindowMap>>,
    pointer_location: Rc<RefCell<(f64, f64)>>,
    pointer_image: ImageBuffer<Rgba<u8>, Vec<u8>>,
    cursor_status: Arc<Mutex<CursorImageStatus>>,
    dnd_icon: Arc<Mutex<Option<wl_surface::WlSurface>>>,
    loop_handle: LoopHandle<Data>,
    notifier: S,
    logger: ::slog::Logger,
}

impl<S: SessionNotifier, Data: 'static> UdevHandlerImpl<S, Data> {
    #[cfg(feature = "egl")]
    pub fn scan_connectors(
        device: &mut RenderDevice,
        egl_buffer_reader: Rc<RefCell<Option<EGLBufferReader>>>,
        logger: &::slog::Logger,
    ) -> HashMap<crtc::Handle, Rc<GliumDrawer<RenderSurface>>> {
        // Get a set of all modesetting resource handles (excluding planes):
        let res_handles = device.resource_handles().unwrap();

        // Use first connected connector
        let connector_infos: Vec<ConnectorInfo> = res_handles
            .connectors()
            .iter()
            .map(|conn| device.get_connector_info(*conn).unwrap())
            .filter(|conn| conn.state() == ConnectorState::Connected)
            .inspect(|conn| info!(logger, "Connected: {:?}", conn.interface()))
            .collect();

        let mut backends = HashMap::new();

        // very naive way of finding good crtc/encoder/connector combinations. This problem is np-complete
        for connector_info in connector_infos {
            let encoder_infos = connector_info
                .encoders()
                .iter()
                .filter_map(|e| *e)
                .flat_map(|encoder_handle| device.get_encoder_info(encoder_handle))
                .collect::<Vec<EncoderInfo>>();
            for encoder_info in encoder_infos {
                for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                    if let Entry::Vacant(entry) = backends.entry(crtc) {
                        let renderer = GliumDrawer::init(
                            device
                                .create_surface(crtc, connector_info.modes()[0], &[connector_info.handle()])
                                .unwrap(),
                            egl_buffer_reader.clone(),
                            logger.clone(),
                        );

                        entry.insert(Rc::new(renderer));
                        break;
                    }
                }
            }
        }

        backends
    }

    #[cfg(not(feature = "egl"))]
    pub fn scan_connectors(
        device: &mut RenderDevice,
        logger: &::slog::Logger,
    ) -> HashMap<crtc::Handle, Rc<GliumDrawer<RenderSurface>>> {
        // Get a set of all modesetting resource handles (excluding planes):
        let res_handles = device.resource_handles().unwrap();

        // Use first connected connector
        let connector_infos: Vec<ConnectorInfo> = res_handles
            .connectors()
            .iter()
            .map(|conn| device.get_connector_info(*conn).unwrap())
            .filter(|conn| conn.state() == ConnectorState::Connected)
            .inspect(|conn| info!(logger, "Connected: {:?}", conn.interface()))
            .collect();

        let mut backends = HashMap::new();

        // very naive way of finding good crtc/encoder/connector combinations. This problem is np-complete
        for connector_info in connector_infos {
            let encoder_infos = connector_info
                .encoders()
                .iter()
                .filter_map(|e| *e)
                .flat_map(|encoder_handle| device.get_encoder_info(encoder_handle))
                .collect::<Vec<EncoderInfo>>();
            for encoder_info in encoder_infos {
                for crtc in res_handles.filter_crtcs(encoder_info.possible_crtcs()) {
                    if !backends.contains_key(&crtc) {
                        let renderer =
                            GliumDrawer::init(device.create_surface(crtc).unwrap(), logger.clone());

                        backends.insert(crtc, Rc::new(renderer));
                        break;
                    }
                }
            }
        }

        backends
    }
}

impl<S: SessionNotifier, Data: 'static> UdevHandlerImpl<S, Data> {
    fn device_added(&mut self, _device: dev_t, path: PathBuf) {
        // Try to open the device
        if let Some(mut device) = self
            .session
            .open(
                &path,
                OFlag::O_RDWR | OFlag::O_CLOEXEC | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            )
            .ok()
            .and_then(
                |fd| match FallbackDevice::new(SessionFd(fd), true, self.logger.clone()) {
                    Ok(drm) => Some(drm),
                    Err(err) => {
                        error!(self.logger, "Skipping drm device, because of error: {}", err);
                        None
                    }
                },
            )
            .and_then(|drm| match GbmDevice::new(drm, self.logger.clone()) {
                Ok(gbm) => Some(gbm),
                Err(err) => {
                    error!(self.logger, "Skipping gbm device, because of error: {}", err);
                    None
                }
            })
            .and_then(|gbm| match EglDevice::new(gbm, self.logger.clone()) {
                Ok(egl) => Some(egl),
                Err(err) => {
                    error!(self.logger, "Skipping egl device, because of error: {}", err);
                    None
                }
            })
        {
            // init hardware acceleration on the primary gpu.
            #[cfg(feature = "egl")]
            {
                if path.canonicalize().ok() == self.primary_gpu {
                    *self.egl_buffer_reader.borrow_mut() =
                        device.bind_wl_display(&*self.display.borrow()).ok();
                }
            }

            #[cfg(feature = "egl")]
            let backends = Rc::new(RefCell::new(UdevHandlerImpl::<S, Data>::scan_connectors(
                &mut device,
                self.egl_buffer_reader.clone(),
                &self.logger,
            )));

            #[cfg(not(feature = "egl"))]
            let backends = Rc::new(RefCell::new(UdevHandlerImpl::<S, Data>::scan_connectors(
                &mut device,
                &self.logger,
            )));

            // Set the handler.
            // Note: if you replicate this (very simple) structure, it is rather easy
            // to introduce reference cycles with Rc. Be sure about your drop order
            let renderer = Rc::new(DrmRenderer {
                compositor_token: self.compositor_token,
                backends: backends.clone(),
                window_map: self.window_map.clone(),
                pointer_location: self.pointer_location.clone(),
                cursor_status: self.cursor_status.clone(),
                dnd_icon: self.dnd_icon.clone(),
                logger: self.logger.clone(),
            });
            let restart_id = self.notifier.register(DrmRendererSessionListener { renderer: renderer.clone(), loop_handle: self.loop_handle.clone() });
            device.set_handler(DrmHandlerImpl { renderer, loop_handle: self.loop_handle.clone() });

            let device_session_id = self.notifier.register(device.observer());
            let dev_id = device.device_id();
            let event_source = device_bind(&self.loop_handle, device)
                .map_err(|e| -> IoError { e.into() })
                .unwrap();

            for renderer in backends.borrow_mut().values() {
                // create cursor
                renderer
                    .borrow()
                    .set_cursor_representation(&self.pointer_image, (2, 2))
                    .unwrap();

                // render first frame
                schedule_initial_render(renderer.clone(), &self.loop_handle);
            }

            self.backends.insert(
                dev_id,
                BackendData {
                    id: device_session_id,
                    restart_id,
                    event_source,
                    surfaces: backends,
                },
            );
        }
    }

    fn device_changed(&mut self, device: dev_t) {
        //quick and dirty, just re-init all backends
        if let Some(ref mut backend_data) = self.backends.get_mut(&device) {
            let logger = &self.logger;
            let pointer_image = &self.pointer_image;
            let egl_buffer_reader = self.egl_buffer_reader.clone();
            let loop_handle = self.loop_handle.clone();
            self.loop_handle
                .with_source(&backend_data.event_source, |source| {
                    let mut backends = backend_data.surfaces.borrow_mut();
                    #[cfg(feature = "egl")]
                    let new_backends = UdevHandlerImpl::<S, Data>::scan_connectors(
                        &mut source.file,
                        egl_buffer_reader,
                        logger,
                    );
                    #[cfg(not(feature = "egl"))]
                    let new_backends = UdevHandlerImpl::<S, Data>::scan_connectors(&mut source.file, logger);
                    *backends = new_backends;

                    for renderer in backends.values() {
                        // create cursor
                        renderer
                            .borrow()
                            .set_cursor_representation(pointer_image, (2, 2))
                            .unwrap();

                        // render first frame
                        schedule_initial_render(renderer.clone(), &loop_handle);
                    }
                });
        }
    }

    fn device_removed(&mut self, device: dev_t) {
        // drop the backends on this side
        if let Some(backend_data) = self.backends.remove(&device) {
            // drop surfaces
            backend_data.surfaces.borrow_mut().clear();
            debug!(self.logger, "Surfaces dropped");

            let device = self.loop_handle.remove(backend_data.event_source).unwrap();

            // don't use hardware acceleration anymore, if this was the primary gpu
            #[cfg(feature = "egl")]
            {
                if device.dev_path().and_then(|path| path.canonicalize().ok()) == self.primary_gpu {
                    *self.egl_buffer_reader.borrow_mut() = None;
                }
            }

            self.notifier.unregister(backend_data.id);
            self.notifier.unregister(backend_data.restart_id);
            debug!(self.logger, "Dropping device");
        }
    }
}

pub struct DrmHandlerImpl<Data: 'static> {
    renderer: Rc<DrmRenderer>,
    loop_handle: LoopHandle<Data>,
}

impl<Data: 'static> DeviceHandler for DrmHandlerImpl<Data> {
    type Device = RenderDevice;

    fn vblank(&mut self, crtc: crtc::Handle) {
        self.renderer.clone().render(crtc, None, Some(&self.loop_handle))
    }
    
    fn error(&mut self, error: <RenderSurface as Surface>::Error) {
        error!(self.renderer.logger, "{:?}", error);
    }
}

pub struct DrmRendererSessionListener<Data: 'static> {
    renderer: Rc<DrmRenderer>,
    loop_handle: LoopHandle<Data>,
}

impl<Data: 'static> SessionObserver for DrmRendererSessionListener<Data> {
    fn pause(&mut self, _device: Option<(u32, u32)>) {}
    fn activate(&mut self, _device: Option<(u32, u32, Option<RawFd>)>) {
        // we want to be called, after all session handling is done (TODO this is not so nice)
        let renderer = self.renderer.clone();
        let handle = self.loop_handle.clone();
        self.loop_handle.insert_idle(move |_| renderer.render_all(Some(&handle)));
    }
}


pub struct DrmRenderer {
    compositor_token: CompositorToken<Roles>,
    backends: Rc<RefCell<HashMap<crtc::Handle, Rc<GliumDrawer<RenderSurface>>>>>,
    window_map: Rc<RefCell<MyWindowMap>>,
    pointer_location: Rc<RefCell<(f64, f64)>>,
    cursor_status: Arc<Mutex<CursorImageStatus>>,
    dnd_icon: Arc<Mutex<Option<wl_surface::WlSurface>>>,
    logger: ::slog::Logger,
}

impl DrmRenderer {
    fn render_all<Data: 'static>(self: Rc<Self>, evt_handle: Option<&LoopHandle<Data>>) {
        for crtc in self.backends.borrow().keys() {
            self.clone().render(*crtc, None, evt_handle);
        }
    }
    fn render<Data: 'static>(self: Rc<Self>, crtc: crtc::Handle, timer: Option<TimerHandle<(std::rc::Weak<DrmRenderer>, crtc::Handle)>>, evt_handle: Option<&LoopHandle<Data>>) {
        if let Some(drawer) = self.backends.borrow().get(&crtc) {
            {
                let (x, y) = *self.pointer_location.borrow();
                let _ = drawer
                    .borrow()
                    .set_cursor_position(x.trunc().abs() as u32, y.trunc().abs() as u32);
            }

            // and draw in sync with our monitor
            let mut frame = drawer.draw();
            frame.clear(None, Some((0.8, 0.8, 0.9, 1.0)), false, Some(1.0), None);
            // draw the surfaces
            drawer.draw_windows(&mut frame, &*self.window_map.borrow(), self.compositor_token);
            let (x, y) = *self.pointer_location.borrow();
            // draw the dnd icon if applicable
            {
                let guard = self.dnd_icon.lock().unwrap();
                if let Some(ref surface) = *guard {
                    if surface.as_ref().is_alive() {
                        drawer.draw_dnd_icon(
                            &mut frame,
                            surface,
                            (x as i32, y as i32),
                            self.compositor_token,
                        );
                    }
                }
            }
            // draw the cursor as relevant
            {
                let mut guard = self.cursor_status.lock().unwrap();
                // reset the cursor if the surface is no longer alive
                let mut reset = false;
                if let CursorImageStatus::Image(ref surface) = *guard {
                    reset = !surface.as_ref().is_alive();
                }
                if reset {
                    *guard = CursorImageStatus::Default;
                }
                if let CursorImageStatus::Image(ref surface) = *guard {
                    drawer.draw_cursor(&mut frame, surface, (x as i32, y as i32), self.compositor_token);
                }
            }

            let result = frame.finish();
            if result.is_ok() {
                // Send frame events so that client start drawing their next frame
                self.window_map.borrow().send_frames(SCOUNTER.next_serial());
            }

            if let Err(err) = result {
                error!(self.logger, "Error during rendering: {:?}", err);
                let reschedule = match err {
                    SwapBuffersError::AlreadySwapped => false,
                    SwapBuffersError::TemporaryFailure(err) => match err.downcast_ref::<smithay::backend::drm::common::Error>() { 
                        Some(&smithay::backend::drm::common::Error::DeviceInactive) => false,
                        Some(&smithay::backend::drm::common::Error::Access { ref source, .. }) if match source.get_ref() {
                            drm::SystemError::PermissionDenied => true,
                            _ => false,
                        } => false,
                        _ => true
                    },
                    SwapBuffersError::ContextLost(err) => panic!("Rendering loop lost: {}", err),
                };

                if reschedule {
                    match (timer, evt_handle) {
                        (Some(handle), _) => {
                            let _ = handle.add_timeout(Duration::from_millis(1000 /*a seconds*/ / 60 /*refresh rate*/), (Rc::downgrade(&self), crtc));
                        },
                        (None, Some(evt_handle)) => {
                            let timer = Timer::new().unwrap();
                            let handle = timer.handle();
                            let _ = handle.add_timeout(Duration::from_millis(1000 /*a seconds*/ / 60 /*refresh rate*/), (Rc::downgrade(&self), crtc));
                            evt_handle.insert_source(timer, |(renderer, crtc), handle, _data| {
                                if let Some(renderer) = renderer.upgrade() {
                                    renderer.render(crtc, Some(handle.clone()), Option::<&LoopHandle<Data>>::None);
                                }
                            }).unwrap();
                        },
                        _ => unreachable!(),
                    }
                }
            } else {
                // Send frame events so that client start drawing their next frame
                self.window_map.borrow().send_frames(SCOUNTER.next_serial());
            }
        }
    }
}
