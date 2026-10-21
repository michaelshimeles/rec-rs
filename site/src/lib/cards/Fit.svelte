<script lang="ts">
	import type { Snippet } from 'svelte';

	/** Lays `children` out at `width`×`height` and scales them to the container's width. */
	let { width, height, children }: { width: number; height: number; children: Snippet } = $props();
</script>

<div class="fit" style:aspect-ratio="{width} / {height}">
	<div class="inner" style:--width="{width}px" style:height="{height}px">
		{@render children()}
	</div>
</div>

<style>
	.fit {
		position: relative;
		width: 100%;
		overflow: hidden;
		container-type: inline-size;
	}

	.inner {
		position: absolute;
		top: 0;
		left: 0;
		width: var(--width);
		transform-origin: 0 0;
		/* tan(atan2(a, b)) is a / b. It fits the drawing in CSS, before and without any script. */
		transform: scale(tan(atan2(100cqw, var(--width))));
	}
</style>
