//! 将 FFmpeg 的 interrupt callback 与 dictionary 打开路径封装为安全接口。
//!
//! `ffmpeg-next` 目前分别提供 dictionary 与 interrupt 两种打开函数。这里的少量
//! FFI 仅复现其上游实现并保证 callback 的所有权在 `AVFormatContext` 生命周期内有效；
//! 其余工程代码不接触 FFmpeg 原始指针。

use std::{
    ffi::CString,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

use ffmpeg_next as ffmpeg;

/// 带中断回调的 FFmpeg 输入上下文。
///
/// 先销毁输入上下文、后释放 callback，确保 FFmpeg 析构期间回调的 opaque 指针仍有效。
pub struct InterruptedInput<F>
where
    F: FnMut() -> bool + 'static,
{
    input: ManuallyDrop<ffmpeg::format::context::Input>,
    callback: *mut F,
}

impl<F> Deref for InterruptedInput<F>
where
    F: FnMut() -> bool + 'static,
{
    type Target = ffmpeg::format::context::Input;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `input` 只会由 Drop 在对象销毁时取出；此前始终有效。
        unsafe { &*(&raw const self.input).cast::<ffmpeg::format::context::Input>() }
    }
}

impl<F> DerefMut for InterruptedInput<F>
where
    F: FnMut() -> bool + 'static,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `input` 只会由 Drop 在对象销毁时取出；这里持有 `&mut self`。
        unsafe { &mut *(&raw mut self.input).cast::<ffmpeg::format::context::Input>() }
    }
}

impl<F> Drop for InterruptedInput<F>
where
    F: FnMut() -> bool + 'static,
{
    fn drop(&mut self) {
        // SAFETY: `input` 与 callback 都由构造函数唯一创建。必须先关闭输入，避免
        // FFmpeg 析构仍访问 callback；随后恰好一次地从 opaque 指针重建 Box。
        unsafe {
            ManuallyDrop::drop(&mut self.input);
            drop(Box::from_raw(self.callback));
        }
    }
}

/// 用同一个 `AVFormatContext` 同时应用输入选项和取消回调。
///
/// 取消闭包返回 `true` 时，FFmpeg 的阻塞打开与读包操作会尽快返回 `AVERROR_EXIT`。
pub fn input_with_dictionary_and_interrupt<F>(
    url: &str,
    options: ffmpeg::Dictionary,
    interrupt: F,
) -> Result<InterruptedInput<F>, ffmpeg::Error>
where
    F: FnMut() -> bool + 'static,
{
    let url = CString::new(url).map_err(|_| ffmpeg::Error::InvalidData)?;
    // SAFETY: 仅本函数操作原始 `AVFormatContext`。成功时 Input 与 callback 的所有权
    // 共同封装进 `InterruptedInput`；失败时按相反顺序显式回收二者。
    unsafe {
        let mut context = ffmpeg::ffi::avformat_alloc_context();
        if context.is_null() {
            return Err(ffmpeg::Error::Bug);
        }
        let interrupt = ffmpeg::util::interrupt::new(Box::new(interrupt));
        let callback = interrupt.interrupt.opaque.cast::<F>();
        (*context).interrupt_callback = interrupt.interrupt;
        let mut options = options.disown();
        let open_result = ffmpeg::ffi::avformat_open_input(
            &mut context,
            url.as_ptr(),
            std::ptr::null_mut(),
            &mut options,
        );
        ffmpeg::Dictionary::own(options);
        if open_result < 0 {
            if !context.is_null() {
                ffmpeg::ffi::avformat_free_context(context);
            }
            drop(Box::from_raw(callback));
            return Err(ffmpeg::Error::from(open_result));
        }
        let info_result = ffmpeg::ffi::avformat_find_stream_info(context, std::ptr::null_mut());
        if info_result < 0 {
            ffmpeg::ffi::avformat_close_input(&mut context);
            drop(Box::from_raw(callback));
            return Err(ffmpeg::Error::from(info_result));
        }
        Ok(InterruptedInput {
            input: ManuallyDrop::new(ffmpeg::format::context::Input::wrap(context)),
            callback,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use super::*;

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    fn empty_wav_path() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "camera-toolbox-ffmpeg-bridge-{}-{}.wav",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let header = [
            b'R', b'I', b'F', b'F', 36, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't', b' ',
            16, 0, 0, 0, 1, 0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0, b'd', b'a',
            b't', b'a', 0, 0, 0, 0,
        ];
        fs::write(&path, header).expect("write minimal WAV fixture");
        path
    }

    #[test]
    fn combined_input_owns_interrupt_callback_until_input_is_destroyed() {
        ffmpeg::init().expect("initialize FFmpeg");
        let path = empty_wav_path();
        let invoked = Arc::new(AtomicBool::new(false));
        let requested = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicUsize::new(0));
        let callback_invoked = Arc::clone(&invoked);
        let callback_requested = Arc::clone(&requested);
        let probe = DropProbe(Arc::clone(&dropped));
        let input = input_with_dictionary_and_interrupt(
            path.to_str().expect("temporary path is UTF-8"),
            ffmpeg::Dictionary::new(),
            move || {
                let _ = &probe;
                callback_invoked.store(true, Ordering::Release);
                callback_requested.load(Ordering::Acquire)
            },
        )
        .expect("open minimal WAV fixture");
        invoked.store(false, Ordering::Release);
        requested.store(true, Ordering::Release);
        unsafe {
            let callback = (*input.as_ptr()).interrupt_callback;
            callback.callback.expect("FFmpeg interrupt callback")(callback.opaque);
        }
        assert!(invoked.load(Ordering::Acquire));
        assert_eq!(dropped.load(Ordering::Acquire), 0);
        drop(input);
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        fs::remove_file(path).expect("remove temporary WAV fixture");
    }
}
