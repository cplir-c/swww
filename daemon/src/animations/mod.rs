use log::error;

use std::{
    sync::Arc,
    thread::{self, Scope},
    time::Instant,
};

use utils::{
    compression::Decompressor,
    ipc::{self, Animation, Answer, BgImg, ImgReq, MmappedBytes},
};

use crate::{
    wallpaper::{AnimationToken, Wallpaper},
    wayland::globals,
};

mod anim_barrier;
mod transitions;
use transitions::Transition;

use self::{anim_barrier::ArcAnimBarrier, transitions::Effect};

///The default thread stack size of 2MiB is way too overkill for our purposes
const STACK_SIZE: usize = 1 << 17; //128KiB

pub(super) struct Animator {
    anim_barrier: ArcAnimBarrier,
}

pub struct TransitionAnimator {
    pub wallpapers: Vec<Arc<Wallpaper>>,
    transition: Transition,
    effect: Effect,
    img: MmappedBytes,
    animation: Option<Animation>,
    now: Instant,
    over: bool,
}

impl TransitionAnimator {
    pub fn new(
        mut wallpapers: Vec<Arc<Wallpaper>>,
        transition: &ipc::Transition,
        img_req: ImgReq,
        animation: Option<Animation>,
    ) -> Option<Self> {
        let ImgReq { img, path, dim, .. } = img_req;
        if wallpapers.is_empty() {
            return None;
        }
        for w in wallpapers.iter_mut() {
            w.set_img_info(BgImg::Img(path.str().to_string()));
        }

        let expect = wallpapers[0].get_dimensions();
        if dim != expect {
            error!("image has wrong dimensions! Expect {expect:?}, actual {dim:?}");
            return None;
        }
        let transition = Transition::new(dim, transition);
        let effect = Effect::new(&transition);
        Some(Self {
            wallpapers,
            transition,
            effect,
            img,
            animation,
            now: Instant::now(),
            over: false,
        })
    }

    pub fn time_to_draw(&self) -> std::time::Duration {
        self.transition.fps.saturating_sub(self.now.elapsed())
    }

    pub fn updt_time(&mut self) {
        self.now = Instant::now();
    }

    pub fn frame(&mut self) -> bool {
        let Self {
            wallpapers,
            transition,
            effect,
            img,
            over,
            ..
        } = self;
        if !*over {
            *over = transition.execute(wallpapers, effect, img.bytes());
            false
        } else {
            true
        }
    }

    pub fn into_image_animator(self) -> Option<ImageAnimator> {
        let Self {
            wallpapers,
            animation,
            ..
        } = self;

        animation.map(|animation| ImageAnimator {
            now: Instant::now(),
            wallpapers,
            animation,
            decompressor: Decompressor::new(),
            i: 0,
        })
    }
}

pub struct ImageAnimator {
    now: Instant,
    pub wallpapers: Vec<Arc<Wallpaper>>,
    animation: Animation,
    decompressor: Decompressor,
    i: usize,
}

impl ImageAnimator {
    pub fn time_to_draw(&self) -> std::time::Duration {
        self.animation.animation[self.i % self.animation.animation.len()]
            .1
            .saturating_sub(self.now.elapsed())
    }

    pub fn updt_time(&mut self) {
        self.now = Instant::now();
    }

    pub fn frame(&mut self) {
        let Self {
            wallpapers,
            animation,
            decompressor,
            i,
            ..
        } = self;

        let frame = &animation.animation[*i % animation.animation.len()].0;

        let mut j = 0;
        while j < wallpapers.len() {
            let result = wallpapers[j].canvas_change(|canvas| {
                decompressor.decompress(frame, canvas, globals::pixel_format())
            });

            if let Err(e) = result {
                error!("failed to unpack frame: {e}");
                wallpapers.swap_remove(j);
                continue;
            }
            j += 1;
        }

        *i += 1;
    }
}

impl Animator {
    pub(super) fn new() -> Self {
        Self {
            anim_barrier: ArcAnimBarrier::new(),
        }
    }

    fn spawn_transition_thread<'a, 'b>(
        scope: &'a Scope<'b, '_>,
        transition: &'b ipc::Transition,
        img: &'b [u8],
        path: &'b str,
        dim: (u32, u32),
        wallpapers: &'b mut Vec<Arc<Wallpaper>>,
    ) where
        'a: 'b,
    {
        thread::Builder::new()
            .name("transition".to_string()) //Name our threads  for better log messages
            .stack_size(STACK_SIZE) //the default of 2MB is way too overkill for this
            .spawn_scoped(scope, move || {
                if wallpapers.is_empty() {
                    return;
                }
                for w in wallpapers.iter_mut() {
                    w.set_img_info(BgImg::Img(path.to_string()));
                }

                let expect = wallpapers[0].get_dimensions();
                if dim != expect {
                    wallpapers.clear();
                    error!("image has wrong dimensions! Expect {expect:?}, actual {dim:?}");
                    return;
                }

                let mut transition = Transition::new(dim, transition);
                let mut effect = Effect::new(&transition);
                while !transition.execute(wallpapers, &mut effect, img) {}
            })
            .unwrap(); // builder only fails if name contains null bytes
    }

    pub(super) fn transition(
        &mut self,
        transition: ipc::Transition,
        imgs: Box<[ImgReq]>,
        animations: Option<Box<[Animation]>>,
        mut wallpapers: Vec<Vec<Arc<Wallpaper>>>,
    ) -> Answer {
        let barrier = self.anim_barrier.clone();
        thread::Builder::new()
            .stack_size(1 << 15)
            .name("animation spawner".to_string())
            .spawn(move || {
                thread::scope(|s| {
                    for (ImgReq { img, path, dim, .. }, wallpapers) in
                        imgs.iter().zip(wallpapers.iter_mut())
                    {
                        Self::spawn_transition_thread(
                            s,
                            &transition,
                            img.bytes(),
                            path.str(),
                            *dim,
                            wallpapers,
                        );
                    }
                });
                drop(imgs);
                #[allow(clippy::drop_non_drop)]
                drop(transition);
                if let Some(animations) = animations {
                    thread::scope(|s| {
                        for (animation, wallpapers) in animations.iter().zip(wallpapers) {
                            let barrier = barrier.clone();
                            Self::spawn_animation_thread(s, animation, wallpapers, barrier);
                        }
                    });
                }
            })
            .unwrap(); // builder only fails if name contains null bytes
        Answer::Ok
    }

    fn spawn_animation_thread<'a, 'b>(
        scope: &'a Scope<'b, '_>,
        animation: &'b Animation,
        mut wallpapers: Vec<Arc<Wallpaper>>,
        barrier: ArcAnimBarrier,
    ) where
        'a: 'b,
    {
        thread::Builder::new()
            .name("animation".to_string()) //Name our threads  for better log messages
            .stack_size(STACK_SIZE) //the default of 2MB is way too overkill for this
            .spawn_scoped(scope, move || {
                /* We only need to animate if we have > 1 frame */
                if animation.animation.len() <= 1 || wallpapers.is_empty() {
                    return;
                }
                log::debug!("Starting animation");

                let mut tokens: Vec<AnimationToken> = wallpapers
                    .iter()
                    .map(|w| w.create_animation_token())
                    .collect();

                let mut now = std::time::Instant::now();

                let mut decompressor = Decompressor::new();
                for (frame, duration) in animation.animation.iter().cycle() {
                    barrier.wait(duration.div_f32(2.0));

                    let mut i = 0;
                    while i < wallpapers.len() {
                        let token = &tokens[i];
                        if !wallpapers[i].has_animation_id(token) {
                            wallpapers.swap_remove(i);
                            tokens.swap_remove(i);
                            continue;
                        }

                        let result = wallpapers[i].canvas_change(|canvas| {
                            decompressor.decompress(frame, canvas, globals::pixel_format())
                        });

                        if let Err(e) = result {
                            error!("failed to unpack frame: {e}");
                            wallpapers.swap_remove(i);
                            tokens.swap_remove(i);
                            continue;
                        }

                        i += 1;
                    }

                    if wallpapers.is_empty() {
                        return;
                    }

                    crate::wallpaper::attach_buffers_and_damage_surfaces(&wallpapers);
                    let timeout = duration.saturating_sub(now.elapsed());
                    crate::spin_sleep(timeout);
                    crate::wallpaper::commit_wallpapers(&wallpapers);

                    now = std::time::Instant::now();
                }
            })
            .unwrap(); // builder only fails if name contains null bytes
    }
}
