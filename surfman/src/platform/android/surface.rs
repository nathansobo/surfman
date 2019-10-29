// surfman/surfman/src/platform/android/surface.rs
//
//! Surface management for Android using the `GraphicBuffer` class and EGL.

use crate::context::ContextID;
use crate::egl::types::{EGLImageKHR, EGLSurface, EGLenum, EGLint};
use crate::gl::types::{GLenum, GLint, GLuint};
use crate::platform::generic::egl::device::EGL_FUNCTIONS;
use crate::platform::generic::egl::{EGLImageKHR, EGL_EXTENSION_FUNCTIONS};
use crate::platform::generic;
use crate::renderbuffers::Renderbuffers;
use crate::{Error, SurfaceAccess, SurfaceID, SurfaceType, WindowingApiError};
use crate::{egl, gl};
use super::context::{Context, GL_FUNCTIONS};
use super::device::Device;
use super::ffi::{AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM, AHARDWAREBUFFER_USAGE_CPU_READ_NEVER};
use super::ffi::{AHARDWAREBUFFER_USAGE_CPU_WRITE_NEVER, AHARDWAREBUFFER_USAGE_GPU_FRAMEBUFFER};
use super::ffi::{AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE, AHardwareBuffer, AHardwareBuffer_Desc};
use super::ffi::{AHardwareBuffer_allocate, AHardwareBuffer_release, ANativeWindow};
use super::ffi::{ANativeWindow_getHeight, ANativeWindow_getWidth};

use euclid::default::Size2D;
use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr;
use std::thread;

// FIXME(pcwalton): Is this right, or should it be `TEXTURE_EXTERNAL_OES`?
const SURFACE_GL_TEXTURE_TARGET: GLenum = gl::TEXTURE_2D;

pub struct Surface {
    pub(crate) context_id: ContextID,
    pub(crate) size: Size2D<i32>,
    pub(crate) objects: SurfaceObjects,
    pub(crate) destroyed: bool,
}

pub struct SurfaceTexture {
    pub(crate) surface: Surface,
    pub(crate) local_egl_image: EGLImageKHR,
    pub(crate) texture_object: GLuint,
    pub(crate) phantom: PhantomData<*const ()>,
}

pub(crate) enum SurfaceObjects {
    HardwareBuffer {
        hardware_buffer: *mut AHardwareBuffer,
        egl_image: EGLImageKHR,
        framebuffer_object: GLuint,
        texture_object: GLuint,
        renderbuffers: Renderbuffers,
    },
    Window {
        egl_surface: EGLSurface,
    },
}

unsafe impl Send for Surface {}

impl Debug for Surface {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(formatter, "Surface({:x})", self.id().0)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if !self.destroyed && !thread::panicking() {
            panic!("Should have destroyed the surface first with `destroy_surface()`!")
        }
    }
}

pub struct NativeWidget {
    pub(crate) native_window: *mut ANativeWindow,
}

impl Device {
    pub fn create_surface(&mut self,
                          context: &Context,
                          _: SurfaceAccess,
                          surface_type: &SurfaceType<NativeWidget>)
                          -> Result<Surface, Error> {
        match *surface_type {
            SurfaceType::Generic { ref size } => self.create_generic_surface(context, size),
            SurfaceType::Widget { ref native_widget } => {
                unsafe {
                    self.create_window_surface(context, native_widget.native_window)
                }
            }
        }
    }

    fn create_generic_surface(&mut self, context: &Context, size: &Size2D<i32>)
                              -> Result<Surface, Error> {
        let _guard = self.temporarily_make_context_current(context)?;

        GL_FUNCTIONS.with(|gl| {
            unsafe {
                // Create a native hardware buffer.
                let hardware_buffer_desc = AHardwareBuffer_Desc {
                    format: AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM,
                    height: size.height as u32,
                    width: size.width as u32,
                    layers: 1,
                    rfu0: 0,
                    rfu1: 0,
                    stride: 10,
                    usage: AHARDWAREBUFFER_USAGE_CPU_READ_NEVER |
                        AHARDWAREBUFFER_USAGE_CPU_WRITE_NEVER |
                        AHARDWAREBUFFER_USAGE_GPU_FRAMEBUFFER |
                        AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE,
                };
                let mut hardware_buffer = ptr::null_mut();
                let result = AHardwareBuffer_allocate(&hardware_buffer_desc, &mut hardware_buffer);
                if result != 0 {
                    return Err(Error::SurfaceCreationFailed(WindowingApiError::Failed));
                }

                // Create an EGL image, and bind it to a texture.
                let egl_image = self.create_egl_image(context, hardware_buffer);

                // Initialize and bind the image to the texture.
                let texture_object =
                    generic::egl::surface::bind_egl_image_to_gl_texture(gl, egl_image);

                // Create the framebuffer, and bind the texture to it.
                let framebuffer_object =
                    gl_utils::create_and_bind_framebuffer(gl,
                                                          SURFACE_GL_TEXTURE_TARGET,
                                                          texture_object);

                // Bind renderbuffers as appropriate.
                let context_descriptor = self.context_descriptor(context);
                let context_attributes = self.context_descriptor_attributes(&context_descriptor);
                let renderbuffers = Renderbuffers::new(gl, size, &context_attributes);
                renderbuffers.bind_to_current_framebuffer(gl);

                debug_assert_eq!(gl.CheckFramebufferStatus(gl::FRAMEBUFFER),
                                 gl::FRAMEBUFFER_COMPLETE);

                Ok(Surface {
                    size: *size,
                    context_id: context.id,
                    objects: SurfaceObjects::HardwareBuffer {
                        hardware_buffer,
                        egl_image,
                        framebuffer_object,
                        texture_object,
                        renderbuffers,
                    },
                    destroyed: false,
                })
            }
        })
    }

    unsafe fn create_window_surface(&mut self,
                                    context: &Context,
                                    native_window: *mut ANativeWindow)
                                    -> Result<Surface, Error> {
        let width = ANativeWindow_getWidth(native_window);
        let height = ANativeWindow_getHeight(native_window);

        let context_descriptor = self.context_descriptor(context);
        let egl_config = self.context_descriptor_to_egl_config(&context_descriptor);

        let egl_surface = EGL_FUNCTIONS::CreateWindowSurface(self.native_display.egl_display(),
                                                             egl_config,
                                                             native_window as *const c_void,
                                                             ptr::null());
        assert_ne!(egl_surface, egl::NO_SURFACE);

        Ok(Surface {
            context_id: context.id,
            size: Size2D::new(width, height),
            objects: SurfaceObjects::Window { egl_surface },
            destroyed: false,
        })
    }

    pub fn create_surface_texture(&self, context: &mut Context, surface: Surface)
                                  -> Result<SurfaceTexture, Error> {
        unsafe {
            match surface.objects {
                SurfaceObjects::Window { .. } => return Err(Error::WidgetAttached),
                SurfaceObjects::HardwareBuffer { hardware_buffer, .. } => {
                    GL_FUNCTIONS.with(|gl| {
                        let _guard = self.temporarily_make_context_current(context)?;
                        let local_egl_image = self.create_egl_image(context, hardware_buffer);
                        let texture_object = generic::egl::surface::bind_egl_image_to_gl_texture(
                            gl,
                            local_egl_image);
                        Ok(SurfaceTexture {
                            surface,
                            local_egl_image,
                            texture_object,
                            phantom: PhantomData,
                        })
                    })
                }
            }
        }
    }

    pub fn present_surface(&self, _: &Context, surface: &mut Surface) -> Result<(), Error> {
        self.present_surface_without_context(surface)
    }

    pub(crate) fn present_surface_without_context(&self, surface: &mut Surface)
                                                  -> Result<(), Error> {
        EGL_FUNCTIONS.with(|egl| {
            unsafe {
                match surface.objects {
                    SurfaceObjects::Window { egl_surface } => {
                        egl.SwapBuffers(self.native_display.egl_display(), egl_surface);
                        Ok(())
                    }
                    SurfaceObjects::HardwareBuffer { .. } => Err(Error::NoWidgetAttached),
                }
            }
        })
    }

    unsafe fn create_egl_image(&self, _: &Context, hardware_buffer: *mut AHardwareBuffer)
                               -> EGLImageKHR {
        // Get the native client buffer.
        let eglGetNativeClientBufferANDROID =
            EGL_EXTENSION_FUNCTIONS.GetNativeClientBufferANDROID
                                   .expect("Where's the `EGL_ANDROID_get_native_client_buffer` \
                                            extension?");
        let client_buffer = eglGetNativeClientBufferANDROID(hardware_buffer);
        assert!(!client_buffer.is_null());

        // Create the EGL image.
        let egl_image_attributes = [
            egl::IMAGE_PRESERVED_KHR as EGLint, egl::TRUE as EGLint,
            egl::NONE as EGLint,                0,
        ];
        let egl_image = (EGL_EXTENSION_FUNCTIONS.CreateImageKHR)(self.native_display.egl_display(),
                                                                 egl::NO_CONTEXT,
                                                                 EGL_NATIVE_BUFFER_ANDROID,
                                                                 client_buffer,
                                                                 egl_image_attributes.as_ptr());
        assert_ne!(egl_image, EGL_NO_IMAGE_KHR);
        egl_image
    }

    pub fn destroy_surface(&self, context: &mut Context, mut surface: Surface)
                           -> Result<(), Error> {
        if context.id != surface.context_id {
            // Leak the surface, and return an error.
            surface.destroyed = true;
            return Err(Error::IncompatibleSurface);
        }

        unsafe {
            match surface.objects {
                SurfaceObjects::HardwareBuffer {
                    ref mut hardware_buffer,
                    ref mut egl_image,
                    ref mut framebuffer_object,
                    ref mut texture_object,
                    ref mut renderbuffers,
                } => {
                    GL_FUNCTIONS.with(|gl| {
                        gl.BindFramebuffer(gl::FRAMEBUFFER, 0);
                        gl.DeleteFramebuffers(1, framebuffer_object);
                        *framebuffer_object = 0;
                        renderbuffers.destroy(gl);

                        gl.DeleteTextures(1, texture_object);
                        *texture_object = 0;

                        let egl_display = self.native_display.egl_display();
                        let result = (EGL_EXTENSION_FUNCTIONS.DestroyImageKHR)(egl_display,
                                                                               *egl_image);
                        assert_ne!(result, egl::FALSE);
                        *egl_image = EGL_NO_IMAGE_KHR;

                        AHardwareBuffer_release(*hardware_buffer);
                        *hardware_buffer = ptr::null_mut();
                    });
                }
                SurfaceObjects::Window { ref mut egl_surface } => {
                    EGL_FUNCTIONS.with(|egl| {
                        egl.DestroySurface(self.native_display.egl_display(), *egl_surface);
                        *egl_surface = egl::NO_SURFACE;
                    })
                }
            }
        }

        surface.destroyed = true;
        Ok(())
    }

    pub fn destroy_surface_texture(&self,
                                   context: &mut Context,
                                   mut surface_texture: SurfaceTexture)
                                   -> Result<Surface, Error> {
        let _guard = self.temporarily_make_context_current(context);
        GL_FUNCTIONS.with(|gl| {
            unsafe {
                gl.DeleteTextures(1, &surface_texture.texture_object);
                surface_texture.texture_object = 0;

                let egl_display = self.native_display.egl_display();
                let result =
                    EGL_EXTENSION_FUNCTIONS.DestroyImageKHR)(egl_display,
                                                             surface_texture.local_egl_image);
                assert_ne!(result, egl::FALSE);
                surface_texture.local_egl_image = EGL_NO_IMAGE_KHR;
            }

            Ok(surface_texture.surface)
        })
    }

    #[inline]
    pub fn lock_surface_data<'s>(&self, surface: &'s mut Surface)
                                 -> Result<SurfaceDataGuard<'s>, Error> {
        Err(Error::Unimplemented)
    }

    #[inline]
    pub fn surface_gl_texture_target(&self) -> GLenum {
        SURFACE_GL_TEXTURE_TARGET
    }
}

impl NativeWidget {
    #[inline]
    pub unsafe fn from_native_window(native_window: *mut ANativeWindow) -> NativeWidget {
        NativeWidget { native_window }
    }
}

impl Surface {
    #[inline]
    pub fn size(&self) -> Size2D<i32> {
        self.size
    }

    pub fn id(&self) -> SurfaceID {
        match self.objects {
            SurfaceObjects::HardwareBuffer { egl_image, .. } => SurfaceID(egl_image as usize),
            SurfaceObjects::Window { egl_surface } => SurfaceID(egl_surface as usize),
        }
    }

    #[inline]
    pub fn context_id(&self) -> ContextID {
        self.context_id
    }
}

impl SurfaceTexture {
    #[inline]
    pub fn gl_texture(&self) -> GLuint {
        self.texture_object
    }
}

pub struct SurfaceDataGuard<'a> {
    phantom: PhantomData<&'a ()>,
}
