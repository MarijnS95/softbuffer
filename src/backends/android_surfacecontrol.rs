//! Implementation of software buffering for Android.
//!
//! This module converts the input buffer into a bitmap and then stretches it to the window.

use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::num::{NonZeroI32, NonZeroU32};
use std::sync::Arc;

use ndk::hardware_buffer::{
    HardwareBuffer, HardwareBufferDesc, HardwareBufferRef, HardwareBufferUsage,
};
use ndk::surface_control::{SurfaceControl, SurfaceTransaction};
use ndk::{hardware_buffer_format::HardwareBufferFormat, native_window::NativeWindow};
#[cfg(doc)]
use raw_window_handle::AndroidNdkWindowHandle;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle};

use crate::error::InitError;
use crate::{BufferInterface, Rect, SoftBufferError, SurfaceInterface};

/// The handle to a window for software buffering.
pub struct AndroidImpl<D, W> {
    native_window: NativeWindow,
    surface_control: Arc<SurfaceControl>,
    window: W,
    _display: PhantomData<D>,
}

// TODO: Implement ContextInterface to guide "fallability" of this API because of Android bug?

impl<D: HasDisplayHandle, W: HasWindowHandle> SurfaceInterface<D, W> for AndroidImpl<D, W> {
    type Context = D;
    type Buffer<'a>
        = BufferImpl
    where
        Self: 'a;

    /// Create a new [`AndroidImpl`] from an [`AndroidNdkWindowHandle`].
    fn new(window: W, _display: &Self::Context) -> Result<Self, InitError<W>> {
        let raw = window.window_handle()?.as_raw();
        let RawWindowHandle::AndroidNdk(a) = raw else {
            return Err(InitError::Unsupported(window));
        };

        // Acquire a new owned reference to the window, that will be freed on drop.
        // SAFETY: We have confirmed that the window handle is valid.
        let native_window = unsafe { NativeWindow::clone_from_ptr(a.a_native_window.cast()) };
        // TODO: Add a SurfaceControl constructor?
        let Some(surface_control) =
            SurfaceControl::create_from_window(&native_window, c"softbuffer")
        else {
            // https://issuetracker.google.com/u/1/issues/320706287
            tracing::error!("Could not create SurfaceControl from (root?) NativeWindow, which requires Android 15");
            return Err(InitError::Unsupported(window));
        };

        Ok(Self {
            native_window,
            surface_control: Arc::new(surface_control),
            _display: PhantomData,
            window,
        })
    }

    #[inline]
    fn window(&self) -> &W {
        &self.window
    }

    /// Also changes the pixel format to [`HardwareBufferFormat::R8G8B8A8_UNORM`].
    fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) -> Result<(), SoftBufferError> {
        let (width, height) = (|| {
            let width = NonZeroI32::try_from(width).ok()?;
            let height = NonZeroI32::try_from(height).ok()?;
            Some((width, height))
        })()
        .ok_or(SoftBufferError::SizeOutOfRange { width, height })?;

        self.native_window
            .set_buffers_geometry(
                width.into(),
                height.into(),
                // Default is typically R5G6B5 16bpp, switch to 32bpp
                Some(HardwareBufferFormat::R8G8B8X8_UNORM),
            )
            .map_err(|err| {
                SoftBufferError::PlatformError(
                    Some("Failed to set buffer geometry on ANativeWindow".to_owned()),
                    Some(Box::new(err)),
                )
            })
    }

    // TODO: Allocate new buffer or return reference from pool WITHOUT mutably borrowing SurfaceInterface!
    fn buffer_mut(&mut self) -> Result<BufferImpl, SoftBufferError> {
        let hardware_buffer = HardwareBuffer::allocate(HardwareBufferDesc {
            width: self.native_window.width() as u32,
            height: self.native_window.height() as u32,
            layers: 1,
            format: HardwareBufferFormat::R8G8B8A8_UNORM,
            usage: HardwareBufferUsage::CPU_WRITE_OFTEN,
            stride: 0, // TODO?
        })
        .map_err(|e| SoftBufferError::PlatformError(None, Some(Box::new(e))))?;

        let desc = hardware_buffer.describe();

        // let native_window_buffer = self.native_window.lock(None).map_err(|err| {
        //     SoftBufferError::PlatformError(
        //         Some("Failed to lock ANativeWindow".to_owned()),
        //         Some(Box::new(err)),
        //     )
        // })?;

        // if !matches!(
        //     native_window_buffer.format(),
        //     // These are the only formats we support
        //     HardwareBufferFormat::R8G8B8A8_UNORM | HardwareBufferFormat::R8G8B8X8_UNORM
        // ) {
        //     return Err(SoftBufferError::PlatformError(
        //         Some(format!(
        //             "Unexpected buffer format {:?}, please call \
        //             .resize() first to change it to RGBx8888",
        //             native_window_buffer.format()
        //         )),
        //         None,
        //     ));
        // }

        let buffer = vec![0; desc.width as usize * desc.height as usize];

        Ok(BufferImpl {
            hardware_buffer,
            buffer,

            surface_control: self.surface_control.clone(),
            // marker: PhantomData,
        })
    }

    /// Fetch the buffer from the window.
    fn fetch(&mut self) -> Result<Vec<u32>, SoftBufferError> {
        Err(SoftBufferError::Unimplemented)
    }
}

pub struct BufferImpl {
    hardware_buffer: HardwareBufferRef,
    buffer: Vec<u32>,
    // TODO: Or have a lifetime after all to store SurfaceControl?
    surface_control: Arc<SurfaceControl>,
    // marker: PhantomData<(&'a D, &'a W)>,
}

// TODO: Move to NativeWindowBufferLockGuard?
unsafe impl Send for BufferImpl {}

impl BufferInterface for BufferImpl {
    fn width(&self) -> NonZeroU32 {
        NonZeroU32::new(self.hardware_buffer.describe().width).unwrap()
    }

    fn height(&self) -> NonZeroU32 {
        NonZeroU32::new(self.hardware_buffer.describe().height).unwrap()
    }

    #[inline]
    fn pixels(&self) -> &[u32] {
        &self.buffer
    }

    #[inline]
    fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.buffer
    }

    #[inline]
    fn age(&self) -> u8 {
        0
    }

    // TODO: This function is pretty slow this way
    fn present(self) -> Result<(), SoftBufferError> {
        const PIXEL_SIZE: usize = 4;

        let desc = self.hardware_buffer.describe();
        dbg!(desc.width, desc.stride);

        let input_lines = self.buffer.chunks(desc.width as usize);

        let output = self
            .hardware_buffer
            .lock(HardwareBufferUsage::CPU_WRITE_MASK, None, None)
            .map_err(|e| {
                SoftBufferError::PlatformError(Some("lock".to_string()), Some(Box::new(e)))
            })?;

        let output = unsafe {
            std::slice::from_raw_parts_mut(
                output.cast::<MaybeUninit<u8>>(),
                desc.stride as usize * PIXEL_SIZE * desc.height as usize,
            )
        };

        let output = output.chunks_mut(desc.stride as usize * PIXEL_SIZE);

        for (output, input) in output.zip(input_lines) {
            let output = &mut output[..desc.width as usize * PIXEL_SIZE];
            // .lines() removed the stride
            assert_eq!(output.len(), input.len() * PIXEL_SIZE);

            for (i, pixel) in input.iter().enumerate() {
                // Swizzle colors from BGR(A) to RGB(A)
                let [b, g, r, a] = pixel.to_le_bytes();
                output[i * PIXEL_SIZE].write(r);
                output[i * PIXEL_SIZE + 1].write(g);
                output[i * PIXEL_SIZE + 2].write(b);
                output[i * PIXEL_SIZE + 3].write(a);
            }
        }

        self.hardware_buffer.unlock().map_err(|e| {
            SoftBufferError::PlatformError(Some("unlock".to_string()), Some(Box::new(e)))
        })?;

        let mut trx = SurfaceTransaction::new();
        // TODO: Pass fence from unlock_async(), which is more efficient
        trx.set_buffer(&self.surface_control, &self.hardware_buffer, None);
        trx.set_on_complete(Box::new(|c| {
            dbg!(c);
        }));
        trx.apply();

        Ok(())
    }

    fn present_with_damage(self, _damage: &[Rect]) -> Result<(), SoftBufferError> {
        // TODO: Android requires the damage rect _at lock time_
        // Since we're faking the backing buffer _anyway_, we could even fake the surface lock
        // and lock it here (if it doesn't influence timings).
        self.present()
    }
}
