import sys
from PIL import Image
im = Image.open(sys.argv[1])
im.save(sys.argv[2])
print(im.size)
