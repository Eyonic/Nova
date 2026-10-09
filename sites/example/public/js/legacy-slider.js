// Old slider nobody links to any more. NOVA reports it as "no reference
// found" so someone can review it; it is never removed automatically.
function legacySlider(element, intervalMilliseconds) {
    var currentIndex = 0;
    var slides = element.querySelectorAll('.slide');
    setInterval(function () {
        slides[currentIndex].classList.remove('active');
        currentIndex = (currentIndex + 1) % slides.length;
        slides[currentIndex].classList.add('active');
    }, intervalMilliseconds);
}
