import { describe, expect, it } from "vite-plus/test";
import { parseReview, reviewUrl } from "./review";

const BASE = "http://example.test/sets/display-tone/review.json";

function doc(overrides: Record<string, unknown> = {}) {
  return {
    schema_version: 1,
    configs: [
      { id: "shoulder", label: "shoulder" },
      { id: "none", label: "none" },
    ],
    images: [
      {
        id: "E1",
        label: "E1 — Ektar",
        renditions: { shoulder: "E1-shoulder.jpg", none: { src: "E1-none.jpg" } },
      },
    ],
    ...overrides,
  };
}

describe("parseReview", () => {
  it("resolves image paths against the review file, not the page", () => {
    const review = parseReview(doc(), BASE);
    const image = review.images[0]!;
    expect(image.renditions.get("shoulder")!.src).toBe(
      "http://example.test/sets/display-tone/E1-shoulder.jpg",
    );
  });

  it("accepts a bare string as the rendition shorthand and mirrors it to preview", () => {
    const rendition = parseReview(doc(), BASE).images[0]!.renditions.get("shoulder")!;
    expect(rendition.preview).toBe(rendition.src);
  });

  it("keeps a distinct preview when one is given", () => {
    const review = parseReview(
      doc({
        images: [
          {
            id: "E1",
            renditions: { shoulder: { src: "big.jpg", preview: "thumb.jpg" } },
          },
        ],
      }),
      BASE,
    );
    const rendition = review.images[0]!.renditions.get("shoulder")!;
    expect(rendition.src).toMatch(/big\.jpg$/);
    expect(rendition.preview).toMatch(/thumb\.jpg$/);
  });

  it("falls back to the id when an image states no label", () => {
    const review = parseReview(
      doc({ images: [{ id: "P4", renditions: { none: "p4.jpg" } }] }),
      BASE,
    );
    expect(review.images[0]!.label).toBe("P4");
  });

  it("allows an image to be missing a rendition", () => {
    const review = parseReview(
      doc({ images: [{ id: "E1", renditions: { shoulder: "a.jpg" } }] }),
      BASE,
    );
    // The comparison is still worth showing; the gap is rendered, not fatal.
    expect(review.images[0]!.renditions.has("none")).toBe(false);
  });

  it("rejects a rendition keyed by an undeclared config, naming the typo", () => {
    expect(() =>
      parseReview(doc({ images: [{ id: "E1", renditions: { shouldre: "a.jpg" } }] }), BASE),
    ).toThrow(/shouldre.*not one of the declared configs \(shoulder, none\)/s);
  });

  it("rejects a rendition that states only one dimension", () => {
    // Half a size reserves nothing, and it would also slip past the load-time
    // check that compares declared against natural dimensions.
    for (const partial of [{ width: 100 }, { height: 100 }]) {
      expect(() =>
        parseReview(
          doc({
            images: [{ id: "E1", renditions: { none: { src: "a.jpg", ...partial } } }],
          }),
          BASE,
        ),
      ).toThrow(/without the other; give both or neither/);
    }
    // Both, or neither, stay valid.
    expect(() =>
      parseReview(
        doc({
          images: [
            {
              id: "E1",
              renditions: { none: { src: "a.jpg", width: 100, height: 50 } },
            },
          ],
        }),
        BASE,
      ),
    ).not.toThrow();
  });

  it("rejects a schema version it cannot read", () => {
    expect(() => parseReview(doc({ schema_version: 2 }), BASE)).toThrow(/schema_version must be 1/);
  });

  it("rejects duplicate config ids", () => {
    expect(() =>
      parseReview(
        doc({
          configs: [
            { id: "a", label: "A" },
            { id: "a", label: "A again" },
          ],
        }),
        BASE,
      ),
    ).toThrow(/two entries with id "a"/);
  });

  it("rejects an empty config list", () => {
    expect(() => parseReview(doc({ configs: [] }), BASE)).toThrow(/at least one/);
  });

  it("names the failing path when a field has the wrong type", () => {
    expect(() =>
      parseReview(doc({ images: [{ id: "E1", renditions: { none: 42 } }] }), BASE),
    ).toThrow(/images\[0\]\.renditions\.none must be an object, got number/);
  });
});

describe("reviewUrl", () => {
  it("defaults to review.json beside the page", () => {
    expect(reviewUrl("http://example.test/review-app/index.html")).toBe(
      "http://example.test/review-app/review.json",
    );
  });

  it("resolves ?data= relative to the page", () => {
    expect(reviewUrl("http://example.test/review-app/?data=../sets/tone/review.json")).toBe(
      "http://example.test/sets/tone/review.json",
    );
  });

  it("accepts an absolute ?data= URL", () => {
    expect(reviewUrl("http://example.test/app/?data=http://other.test/r.json")).toBe(
      "http://other.test/r.json",
    );
  });
});
