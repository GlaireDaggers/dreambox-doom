extern crate lazy_static;
extern crate dbsdk_rs;

use lazy_static::lazy_static;
use std::{ffi::{c_void, c_char, CStr}, ptr::{self, slice_from_raw_parts}, convert::TryFrom, alloc::Layout, sync::RwLock, io::{Read, Seek}};

use dbsdk_rs::{vdp::{self, Color32, TextureFormat, Rectangle, PackedVertex, Texture}, db, io::{self, FileMode, FileStream}, math::{Vector4, Vector2}, gamepad::{Gamepad, self, GamepadSlot, GamepadState, GamepadButtonMask, GamepadButton}, audio::{AudioSample, self}};

const AUDIO_LOOKAHEAD_TIME: f64 = 0.05;

// technically sounds will be buffered up to (AUDIO_LOOKAHEAD_TIME * 2) seconds in advance
// at a lookahead of 0.05s, w/ a buffer size of 512 samples @ 11025 Hz,
// this is enough time to contain just over 2 buffers worth of audio (0.05 / (512.0/11025.0)) * 2 = 2.1533203125
// so we round up and keep refs to the previous 3 buffers of audio to prevent them from being deallocated before they play
const AUDIO_NUM_BUFFERS: usize = 3;

struct MyApp {
    time: f32,
    mx: f32,
    prev_left: bool,
    prev_right: bool,
    prev_up: bool,
    prev_down: bool,
    canvas_tex: Texture,
    prev_gp_state: GamepadState,
    audio_buf: [[Option<AudioSample>;AUDIO_NUM_BUFFERS];2],
    audio_queue: [Option<Vec<i16>>;2],
    audio_schedule_time: f64,
    next_buf: usize,
}

impl MyApp {
    pub fn new() -> MyApp {
        unsafe {
            doom_set_print(doom_print);
            doom_set_malloc(doom_malloc, doom_free);
            doom_set_file_io(doom_open, doom_close, doom_read, doom_write, doom_seek, doom_tell, doom_eof);
            doom_set_gettime(doom_gettime);
            doom_set_exit(doom_exit);
            doom_set_getenv(doom_getenv);
            doom_set_playmus(doom_playmus);
    
            let args = [
            ];
    
            doom_init(args.len() as i32, args.as_ptr(), 0);
        }

        // read & upload soundfont
        {
            let mut sf = FileStream::open("/cd/content/soundfont.sf2", FileMode::Read).unwrap();
            sf.seek(std::io::SeekFrom::End(0)).unwrap();
            let size = sf.position();
            sf.seek(std::io::SeekFrom::Start(0)).unwrap();
            let mut sf_buf: Vec<u8> = vec![0;size as usize];
            sf.read_exact(&mut sf_buf).unwrap();

            audio::init_synth(&sf_buf).unwrap();

            db::log("Synth initialized");
        }

        return MyApp {
            time: 0.0,
            mx: 0.0,
            prev_left: false,
            prev_right: false,
            prev_up: false,
            prev_down: false,
            canvas_tex: Texture::new(512, 256, false, TextureFormat::RGBA8888).unwrap(),
            prev_gp_state: GamepadState { button_mask: GamepadButtonMask::none(), left_stick_x: 0, left_stick_y: 0, right_stick_x: 0, right_stick_y: 0 },
            audio_buf: [[None, None, None], [None, None, None]],
            audio_queue: [None, None],
            audio_schedule_time: -1.0,
            next_buf: 0
        };
    }

    fn schedule_voice(handle: i32, slot: i32, pan: f32, t: f64) {
        audio::queue_set_voice_param_i(slot, audio::AudioVoiceParam::SampleData, handle, t);
        audio::queue_set_voice_param_i(slot, audio::AudioVoiceParam::Samplerate, 11025, t);
        audio::queue_set_voice_param_i(slot, audio::AudioVoiceParam::LoopEnabled, 0, t);
        audio::queue_set_voice_param_i(slot, audio::AudioVoiceParam::Reverb, 0, t);
        audio::queue_set_voice_param_f(slot, audio::AudioVoiceParam::Volume, 1.0, t);
        audio::queue_set_voice_param_f(slot, audio::AudioVoiceParam::Pitch, 1.0, t);
        audio::queue_set_voice_param_f(slot, audio::AudioVoiceParam::Detune, 0.0, t);
        audio::queue_set_voice_param_f(slot, audio::AudioVoiceParam::Pan, pan, t);
        audio::queue_set_voice_param_f(slot, audio::AudioVoiceParam::FadeInDuration, 0.0, t);
        audio::queue_set_voice_param_f(slot, audio::AudioVoiceParam::FadeOutDuration, 0.0, t);

        audio::queue_stop_voice(slot, t);
        audio::queue_start_voice(slot, t);
    }

    fn process_audio(&mut self) {
        let sample_cnt = 512;
        let t = self.audio_schedule_time + AUDIO_LOOKAHEAD_TIME;

        // we need to "unzip" interleaved LR audio into two mono buffers
        let mut data_l: Vec<i16> = vec![0;sample_cnt];
        let mut data_r: Vec<i16> = vec![0;sample_cnt];
        
        // get audio buffer from DOOM
        unsafe {
            let audio_buf_ptr = doom_get_sound_buffer();
            let audio_buf = slice_from_raw_parts(audio_buf_ptr, 1024);

            for i in 0..sample_cnt {
                data_l[i] = (&*audio_buf)[i * 2] << 2;
                data_r[i] = (&*audio_buf)[i * 2 + 1] << 2;
            }
        }

        // we have a rotating buffer of audio samples we use to upload audio data
        // NOTE: this will automatically deallocate the previous buffers here

        // this is a little tricky:
        // basically, instead of queueing audio chunks right away, we actually stuff them into a buffer and wait
        // then, when we get the next buffer, we actually take its first sample and append it to the start of the LAST buffer and submit that
        // this is all to make DreamBox's 2-tap sampling play nicely - b/c at the end of one of our submitted samples, DreamBox doesn't take the next sample we queue up into account,
        // so there's a single sample of aliasing in between every single buffer we submit and it ends up sounding scratchy
        // this fixes that by basically making each buffer end with the next buffer's starting sample

        match &mut self.audio_queue[0] {
            Some(v1) => {
                // had a previous buffer, append the first sample of this new buffer to the end and queue that
                v1.push(data_l[0]);
                let newbuf_l = AudioSample::create_s16(v1, 11025).expect("Failed creating audio sample");
                let handle_l = newbuf_l.handle;
                self.audio_buf[0][self.next_buf % AUDIO_NUM_BUFFERS] = Some(newbuf_l);
                MyApp::schedule_voice(handle_l, 0, -1.0, t);
            }
            None => {
            }
        }

        match &mut self.audio_queue[1] {
            Some(v2) => {
                // had a previous buffer, append the first sample of this new buffer to the end and queue that
                v2.push(data_r[0]);
                let newbuf_r = AudioSample::create_s16(v2, 11025).expect("Failed creating audio sample");
                let handle_r = newbuf_r.handle;
                self.audio_buf[1][self.next_buf % AUDIO_NUM_BUFFERS] = Some(newbuf_r);
                MyApp::schedule_voice(handle_r, 1, 1.0, t);
            }
            None => {
            }
        }

        // replace audio in the queue with new chunk
        self.audio_queue[0] = Some(data_l);
        self.audio_queue[1] = Some(data_r);

        self.next_buf += 1;
    }

    pub fn update(&mut self) {
        let delta = 1.0 / 60.0;

        let gp = Gamepad::new(GamepadSlot::SlotA);
        let new_state = gp.read_state();
        let prev_state = self.prev_gp_state;
        self.prev_gp_state = new_state;

        // we don't actually have a real clock, so we're going to just lie to DOOM about what time it is lol
        self.time += delta;
        unsafe {
            TIME = self.time;
        }

        if self.audio_schedule_time < audio::get_time() {
            db::log(format!("Audio schedule time fell behind real time, recovering...").as_str());
            self.audio_schedule_time = audio::get_time();
        }

        // NOTE: DOOM audio is 11025 Hz, 512 samples * 2 channels per buffer
        if audio::get_time() >= self.audio_schedule_time - AUDIO_LOOKAHEAD_TIME {
            self.process_audio();
            self.audio_schedule_time += 512.0 / 11025.0;
        }

        unsafe {
            if new_state.is_pressed(gamepad::GamepadButton::R2) && !prev_state.is_pressed(gamepad::GamepadButton::R2) {
                doom_key_down(0x80 + 0x1d);
            }
            else if !new_state.is_pressed(GamepadButton::R2) && prev_state.is_pressed(GamepadButton::R2) {
                doom_key_up(0x80 + 0x1d);
            }

            if new_state.is_pressed(gamepad::GamepadButton::L2) && !prev_state.is_pressed(gamepad::GamepadButton::L2) {
                doom_key_down(0x80 + 0x36);
            }
            else if !new_state.is_pressed(GamepadButton::L2) && prev_state.is_pressed(GamepadButton::L2) {
                doom_key_up(0x80 + 0x36);
            }

            if new_state.is_pressed(gamepad::GamepadButton::R1) && !prev_state.is_pressed(gamepad::GamepadButton::R1) {
                doom_key_down(101);
            }
            else if !new_state.is_pressed(GamepadButton::R1) && prev_state.is_pressed(GamepadButton::R1) {
                doom_key_up(101);
            }

            if new_state.is_pressed(gamepad::GamepadButton::L1) && !prev_state.is_pressed(gamepad::GamepadButton::L1) {
                doom_key_down(113);
            }
            else if !new_state.is_pressed(GamepadButton::L1) && prev_state.is_pressed(GamepadButton::L1) {
                doom_key_up(113);
            }

            if new_state.is_pressed(gamepad::GamepadButton::Start) && !prev_state.is_pressed(gamepad::GamepadButton::Start) {
                doom_key_down(0xff);
            }
            else if !new_state.is_pressed(GamepadButton::Start) && prev_state.is_pressed(GamepadButton::Start) {
                doom_key_up(0xff);
            }

            if new_state.is_pressed(gamepad::GamepadButton::A) && !prev_state.is_pressed(gamepad::GamepadButton::A) {
                doom_key_down(32);
                doom_key_down(13);
            }
            else if !new_state.is_pressed(GamepadButton::A) && prev_state.is_pressed(GamepadButton::A) {
                doom_key_up(32);
                doom_key_up(13);
            }

            if new_state.is_pressed(gamepad::GamepadButton::B) && !prev_state.is_pressed(gamepad::GamepadButton::B) {
                doom_key_down(127);
            }
            else if !new_state.is_pressed(GamepadButton::B) && prev_state.is_pressed(GamepadButton::B) {
                doom_key_up(127);
            }

            if new_state.is_pressed(gamepad::GamepadButton::X) && !prev_state.is_pressed(gamepad::GamepadButton::X) {
                doom_key_down(27);
            }
            else if !new_state.is_pressed(GamepadButton::X) && prev_state.is_pressed(GamepadButton::X) {
                doom_key_up(27);
            }

            if new_state.is_pressed(gamepad::GamepadButton::Select) && !prev_state.is_pressed(gamepad::GamepadButton::Select) {
                doom_key_down(9);
            }
            else if !new_state.is_pressed(GamepadButton::Select) && prev_state.is_pressed(GamepadButton::Select) {
                doom_key_up(9);
            }

            let prev_mx = self.mx as i32;
            self.mx += (new_state.right_stick_x as f32 / 32767.0) * delta * 4096.0;
            let new_mx = self.mx as i32;

            doom_mouse_move(new_mx - prev_mx, 0);

            let new_left = new_state.left_stick_x < -1024 || new_state.is_pressed(GamepadButton::Left);
            let new_right = new_state.left_stick_x > 1024 || new_state.is_pressed(GamepadButton::Right);

            let new_up = new_state.left_stick_y > 1024 || new_state.is_pressed(GamepadButton::Up);
            let new_down = new_state.left_stick_y < -1024 || new_state.is_pressed(GamepadButton::Down);

            if new_left && !self.prev_left {
                doom_key_down(44);
            }
            else if !new_left && self.prev_left {
                doom_key_up(44);
            }

            if new_right && !self.prev_right {
                doom_key_down(46);
            }
            else if !new_right && self.prev_right {
                doom_key_up(46);
            }

            if new_up && !self.prev_up {
                doom_key_down(0xad);
            }
            else if !new_up && self.prev_up {
                doom_key_up(0xad);
            }

            if new_down && !self.prev_down {
                doom_key_down(0xaf);
            }
            else if !new_down && self.prev_down {
                doom_key_up(0xaf);
            }

            self.prev_left = new_left;
            self.prev_right = new_right;
            self.prev_up = new_up;
            self.prev_down = new_down;

            doom_update();

            // update screen texture
            let fb_data = doom_get_framebuffer(4) as *const u8;
            let fb_data_slice = std::slice::from_raw_parts(fb_data, 320 * 200 * 4);

            let update_rect = Rectangle::new(0, 0, 320, 200);

            self.canvas_tex.set_texture_data_region(0, Some(update_rect), fb_data_slice);
        }

        vdp::clear_color(Color32::new(0, 0, 0, 255));

        let vertex_data = [
            PackedVertex::new(Vector4::new(-1.0, -1.0, 0.0, 1.0), Vector2::new(0.0, 0.78125), Color32::new(255, 255, 255, 255), Color32::new(0, 0, 0, 0)),
            PackedVertex::new(Vector4::new(1.0, -1.0, 0.0, 1.0), Vector2::new(0.625, 0.78125), Color32::new(255, 255, 255, 255), Color32::new(0, 0, 0, 0)),
            PackedVertex::new(Vector4::new(-1.0, 1.0, 0.0, 1.0), Vector2::new(0.0, 0.0), Color32::new(255, 255, 255, 255), Color32::new(0, 0, 0, 0)),

            PackedVertex::new(Vector4::new(-1.0, 1.0, 0.0, 1.0), Vector2::new(0.0, 0.0), Color32::new(255, 255, 255, 255), Color32::new(0, 0, 0, 0)),
            PackedVertex::new(Vector4::new(1.0, -1.0, 0.0, 1.0), Vector2::new(0.625, 0.78125), Color32::new(255, 255, 255, 255), Color32::new(0, 0, 0, 0)),
            PackedVertex::new(Vector4::new(1.0, 1.0, 0.0, 1.0), Vector2::new(0.625, 0.0), Color32::new(255, 255, 255, 255), Color32::new(0, 0, 0, 0))
        ];
        {
            vdp::bind_texture(Some(&self.canvas_tex));
        }
        vdp::draw_geometry_packed(vdp::Topology::TriangleList, &vertex_data);
    }
}

static mut TIME: f32 = 0.0;

lazy_static! {
    static ref MY_APP: RwLock<MyApp> = RwLock::new(MyApp::new());
}

extern {
    fn doom_set_print(print_fn: unsafe extern "C" fn(str: *const c_char));
    fn doom_set_malloc(malloc_fn: unsafe extern "C" fn(size: i32) -> *mut c_void, free_fn: unsafe extern "C" fn(ptr: *mut c_void));
    fn doom_set_file_io(open_fn: unsafe extern "C" fn(filename: *const c_char, mode: *const c_char) -> i32,
        close_fn: unsafe extern "C" fn(handle: i32),
        read_fn: unsafe extern "C" fn(handle: i32, buf: *mut c_void, count: i32) -> i32,
        write_fn: unsafe extern "C" fn(handle: i32, buf: *const c_void, count: i32) -> i32,
        seek_fn: unsafe extern "C" fn(handle: i32, offset: i32, origin: i32) -> i32,
        tell_fn: unsafe extern "C" fn(handle: i32) -> i32,
        eof_fn: unsafe extern "C" fn(handle: i32) -> i32);
    fn doom_set_gettime(gettime_fn: unsafe extern "C" fn(sec: *mut i32, usec: *mut i32));
    fn doom_set_exit(exit_fn: unsafe extern "C" fn(code: i32));
    fn doom_set_getenv(getenv_fn: unsafe extern "C" fn(var: *const c_char) -> *const c_char);
    fn doom_set_playmus(playmus_fn: unsafe extern "C" fn(id: *const c_char, looping: i32));

    fn doom_init(argc: i32, argv: *const *const c_char, flags: i32);
    fn doom_update();

    fn doom_key_down(key: i32);
    fn doom_key_up(key: i32);
    fn doom_mouse_move(dx: i32, dy: i32);

    fn doom_get_framebuffer(channels: i32) -> *const c_void;
    fn doom_get_sound_buffer() -> *const i16;
}

extern {
    fn fs_open(pathstr: *const c_char, mode: io::FileMode) -> i32;
    fn fs_read(handle: i32, buffer: *mut c_void, bufferLen: i32) -> i32;
    fn fs_write(handle: i32, buffer: *const c_void, bufferLen: i32) -> i32;
    fn fs_seek(handle: i32, position: i32, whence: io::SeekOrigin) -> i32;
    fn fs_tell(handle: i32) -> i32;
    fn fs_close(handle: i32);
    fn fs_eof(handle: i32) -> bool;
}

fn tick() {
    let mut my_app = MY_APP.write().unwrap();
    my_app.update();
}

unsafe extern "C" fn doom_playmus(id: *const c_char, looping: i32) {
    let mus_id = CStr::from_ptr(id).to_str().unwrap();
    let path = format!("/cd/content/midi/{}.mid", mus_id);
    db::log(format!("PLAY MUSIC: {}", path).as_str());

    let mut midi_file = match FileStream::open(path.as_str(), FileMode::Read) {
        Ok(v) => v,
        _ => {
            audio::set_midi_volume(0.0);
            return;
        }
    };

    midi_file.seek(std::io::SeekFrom::End(0)).unwrap();
    let size = midi_file.position();
    midi_file.seek(std::io::SeekFrom::Start(0)).unwrap();
    let mut midi_buf: Vec<u8> = vec![0;size as usize];
    midi_file.read_exact(&mut midi_buf).unwrap();

    audio::set_midi_volume(0.2);
    audio::play_midi(&midi_buf, looping != 0).unwrap();
}

unsafe extern "C" fn doom_print(str: *const c_char) {
    let c_str = CStr::from_ptr(str);
    db::log(c_str.to_str().unwrap());
}

unsafe extern "C" fn doom_malloc(size: i32) -> *mut c_void {
    // basically we just allocate a block of memory with an 8-byte preamble that stores the length (we use 8 bytes to maintain alignment) 
    // that way, we can pass the raw pointer to C, and then when we get the pointer back we do some arithmetic to get at the original preamble
    // and then we can reconstruct the Layout that was passed to alloc

    // NOTE: we align to align_of::<i64>() which is the equivalent of C's max_align_t for wasm32
    // this matches the behavior of C's malloc

    // NOTE: removed write_unaligned b/c it is no longer necessary - malloc is already 8-byte aligned

    let actual_size = 8 + usize::try_from(size).unwrap();
    let layout = Layout::array::<u8>(actual_size).unwrap().align_to(8).unwrap();
    let mem = unsafe { std::alloc::alloc(layout) };
    if !mem.is_null() {
        unsafe { mem.cast::<i64>().write(size.into()) };
    }
    unsafe { mem.add(8) }.cast()
}

unsafe extern "C" fn doom_free(ptr: *mut c_void) {
    // back up by 8 bytes to get at the preamble, which contains the allocated size

    // NOTE: removed read_unaligned b/c it is no longer necessary - malloc is already 8-byte aligned

    let ptr = unsafe { ptr.sub(8) }.cast::<u8>();
    let size = unsafe { ptr.cast::<i64>().read() };
    let actual_size = 8 + usize::try_from(size).unwrap();
    let layout = Layout::array::<u8>(actual_size).unwrap().align_to(8).unwrap();
    unsafe { std::alloc::dealloc(ptr, layout) };
}

unsafe extern "C" fn doom_open(filename: *const c_char, mode: *const c_char) -> i32 {
    let mode_str = CStr::from_ptr(mode).to_str().unwrap();

    let filemode = match mode_str {
        "r" => FileMode::Read,
        "rb" => FileMode::Read,
        "w" => FileMode::Write,
        "wb" => FileMode::Write,
        _ => panic!("Unexpected file mode ({})", mode_str)
    };

    return fs_open(filename, filemode);
}

unsafe extern "C" fn doom_close(handle: i32) {
    fs_close(handle);
}

unsafe extern "C" fn doom_read(handle: i32, buf: *mut c_void, count: i32) -> i32 {
    return fs_read(handle, buf, count);
}

unsafe extern "C" fn doom_write(handle: i32, buf: *const c_void, count: i32) -> i32 {
    return fs_write(handle, buf, count);
}

unsafe extern "C" fn doom_seek(handle: i32, offset: i32, origin: i32) -> i32 {
    match origin {
        0 => {
            return fs_seek(handle, offset, io::SeekOrigin::Begin);
        }
        1 => {
            return fs_seek(handle, offset, io::SeekOrigin::Current);
        }
        2 => {
            return fs_seek(handle, offset, io::SeekOrigin::End);
        }
        _ => {
            panic!("Unexpected seek origin");
        }
    }
}

unsafe extern "C" fn doom_tell(handle: i32) -> i32 {
    return fs_tell(handle);
}

unsafe extern "C" fn doom_eof(handle: i32) -> i32 {
    return if fs_eof(handle) { 1 } else { 0 };
}

unsafe extern "C" fn doom_exit(code: i32) {
    panic!("TODO: exit - code: {}", code);
}

unsafe extern "C" fn doom_gettime(sec: *mut i32, usec: *mut i32) {
    let total_sec = TIME.floor();
    let sec_rem = TIME - total_sec;

    *sec = total_sec as i32;
    *usec = (sec_rem * 1000000.0) as i32;
}

unsafe extern "C" fn doom_getenv(var: *const c_char) -> *const c_char {
    let var_str = CStr::from_ptr(var).to_str().unwrap();
    match var_str {
        "DOOMWADDIR" => {
            return b"/cd/content\0".as_ptr() as *const c_char;
        },
        "HOME" => {
            return b"/ma\0".as_ptr() as *const c_char;
        },
        _ => {
            return ptr::null();
        }
    };
}

#[no_mangle]
pub fn main(_: i32, _: i32) -> i32 {
    db::register_panic();
    vdp::set_vsync_handler(Some(tick));
    return 0;
}