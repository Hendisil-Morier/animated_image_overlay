//! Integration tests verifying pixel premultiplication, Lanczos3 downscaling, and SWAR rotation.

use animated_image_widget::decode::{
    resize_premultiplied_argb8888, rotate_argb8888_into, straight_rgba_to_premultiplied_argb8888,
};
use fast_image_resize::Resizer;

#[test]
fn decode_converts_straight_rgba_to_premultiplied_argb8888()
{
    let mut pixels = vec![
        200, 100, 50, 255, // opaque
        200, 100, 50, 0, // fully transparent clears RGB
        200, 100, 50, 128, // semi-transparent premultiplied
    ];
    straight_rgba_to_premultiplied_argb8888(&mut pixels);

    assert_eq!(&pixels[0..4], &[50, 100, 200, 255]);
    assert_eq!(&pixels[4..8], &[0, 0, 0, 0]);
    // ((50 * 128) + 127) / 255 = 25, ((100 * 128) + 127) / 255 = 50, ((200 * 128) + 127) / 255 = 100
    assert_eq!(&pixels[8..12], &[25, 50, 100, 128]);
}

#[test]
fn rotate_argb8888_handles_chirality_and_transparent_apron_antialiasing()
{
    // 2x2 source pixels in [B, G, R, A]:
    // (0,0)=[1,0,0,255], (1,0)=[2,0,0,255]
    // (0,1)=[3,0,0,255], (1,1)=[4,0,0,255]
    let grid: [u8; 16] = [
        1, 0, 0, 255, 2, 0, 0, 255, // row 0
        3, 0, 0, 255, 4, 0, 0, 255, // row 1
    ];
    let mut dst_small = [0_u8; 16];

    // 90 deg CW unflipped -> [3, 1; 4, 2]
    rotate_argb8888_into(&grid, 2, 2, 90.0, false, 2, 2, &mut dst_small).unwrap();
    assert_eq!(
        dst_small,
        [
            3, 0, 0, 255, 1, 0, 0, 255, // row 0
            4, 0, 0, 255, 2, 0, 0, 255, // row 1
        ]
    );

    // 90 deg CW flipped horizontally -> [1, 3; 2, 4]
    rotate_argb8888_into(&grid, 2, 2, 90.0, true, 2, 2, &mut dst_small).unwrap();
    assert_eq!(
        dst_small,
        [
            1, 0, 0, 255, 3, 0, 0, 255, // row 0
            2, 0, 0, 255, 4, 0, 0, 255, // row 1
        ]
    );

    // 20x10 solid opaque colored block rotated 45 deg preserves center alpha=255 and valid premul edge alpha.
    let block = [25, 100, 200, 255].repeat(20 * 10);
    let mut dst_full = vec![0_u8; 22 * 22 * 4];
    rotate_argb8888_into(&block, 20, 10, 45.0, false, 22, 22, &mut dst_full).unwrap();

    // Exterior corner lies strictly outside source box.
    assert_eq!(&dst_full[0..4], &[0, 0, 0, 0]);

    // Center pixel remains solid opaque with exact source channels.
    let mid_off = (11 * 22 + 11) * 4;
    assert_eq!(&dst_full[mid_off..mid_off + 4], &[25, 100, 200, 255]);

    // Partial apron alpha exists along boundaries and every output pixel stays premultiplied-valid.
    let mut has_semitransparent = false;
    for px in dst_full.as_chunks::<4>().0
    {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        assert!(b <= a && g <= a && r <= a, "invalid premul pixel {px:?}");
        if a > 0 && a < 255
        {
            has_semitransparent = true;
        }
    }
    assert!(has_semitransparent);

    // Scaled physical target (11x11) preserves centered foreground pixels and premultiplied edges.
    let mut dst_half = vec![0_u8; 11 * 11 * 4];
    rotate_argb8888_into(&block, 20, 10, 45.0, false, 11, 11, &mut dst_half).unwrap();
    assert_eq!(&dst_half[0..4], &[0, 0, 0, 0]);
    let half_mid = (5 * 11 + 5) * 4;
    assert_eq!(&dst_half[half_mid..half_mid + 4], &[25, 100, 200, 255]);
    for px in dst_half.as_chunks::<4>().0
    {
        let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
        assert!(b <= a && g <= a && r <= a, "invalid premul pixel {px:?}");
    }

    // Degenerate 1x1 source and mirrored fractional sweep verify interior/apron boundary transitions.
    let single = [40_u8, 80, 120, 200];
    let mut dst_1x1 = vec![0_u8; 4 * 4 * 4];
    rotate_argb8888_into(&single, 1, 1, 33.0, false, 4, 4, &mut dst_1x1).unwrap();
    for px in dst_1x1.as_chunks::<4>().0
    {
        assert!(px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]);
    }

    let mut gradient = vec![0_u8; 16 * 12 * 4];
    for (idx, px) in gradient.as_chunks_mut::<4>().0.iter_mut().enumerate()
    {
        let b = ((idx * 17) % 256) as u8;
        let g = ((idx * 31) % 256) as u8;
        let r = ((idx * 53) % 256) as u8;
        *px = [b, g, r, 255];
    }
    let mut dst_grad = vec![0_u8; 24 * 24 * 4];
    for angle in [37.5, 143.0, 225.0, 310.0]
    {
        rotate_argb8888_into(&gradient, 16, 12, angle, true, 24, 24, &mut dst_grad).unwrap();
        let center_off = (12 * 24 + 12) * 4;
        assert_eq!(dst_grad[center_off + 3], 255);
    }
}

#[test]
fn resize_premultiplied_argb8888_downscales_and_preserves_identity()
{
    let mut resizer = Resizer::new();
    let solid: Vec<u8> = [30_u8, 90, 180, 255]
        .into_iter()
        .cycle()
        .take(20 * 10 * 4)
        .collect();

    // Matching source and destination dimensions copy bytes identically without Lanczos filtering.
    let same = resize_premultiplied_argb8888(&solid, 20, 10, 20, 10, &mut resizer).unwrap();
    assert_eq!(same, solid);

    // Downscaling halves dimensions while preserving premultiplied channel ordering (`<= A`).
    let small = resize_premultiplied_argb8888(&solid, 20, 10, 10, 5, &mut resizer).unwrap();
    assert_eq!(small.len(), 10 * 5 * 4);
    assert_eq!(&small[0..4], &[30, 90, 180, 255]);

    let semi: Vec<u8> = [25_u8, 60, 110, 128]
        .into_iter()
        .cycle()
        .take(20 * 10 * 4)
        .collect();
    let small_semi = resize_premultiplied_argb8888(&semi, 20, 10, 10, 5, &mut resizer).unwrap();
    assert_eq!(&small_semi[0..4], &[25, 60, 110, 128]);

    // Mismatched source byte counts return an error rather than resizing invalid memory.
    assert!(resize_premultiplied_argb8888(&solid[..16], 20, 10, 10, 5, &mut resizer).is_err());
}
