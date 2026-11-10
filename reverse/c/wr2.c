/* wr2.c —— 官方 swtcon 规范的渲染器
 * 几何: 260 cell/行, 1408 行(= 4 preamble + 1404 面板列), 17 槽, pixclock 0x7080
 * 面板坐标: panel_col = 1403 - user_x, panel_row = 1871 - user_y
 * cell: 低16位 = 8 个面板行的 2bit 驱动码, 高16位 = 命令字节
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <fcntl.h>
#include <unistd.h>
#include <time.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <linux/fb.h>
#include "wbf_lut.h"

#define CELLS 260
#define LINE  (CELLS*4)
#define ROWS  1408
#define SLOT  (LINE*ROWS)
#define NSLOT 17
#define UW    1404
#define UH    1872
#define COLS  1404
#define CSTR  234                 /* 内容 cell 数 = 1872/8 */
#define PDROW (CSTR*2)            /* 每列(面板行)的 16bit 码字节数 */

static int fb_fd=-1; static unsigned char *fbmem; static struct fb_var_screeninfo vi;
static unsigned char img[UW*UH];
static unsigned char prv[UW*UH];

static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}

static int setmode(uint32_t xr,uint32_t yr,uint32_t yv,uint32_t pixc,uint32_t lo){
    struct fb_var_screeninfo v; memset(&v,0,sizeof v);
    ioctl(fb_fd,FBIOGET_VSCREENINFO,&v);
    v.xres=xr; v.yres=yr; v.xres_virtual=xr; v.yres_virtual=yv;
    v.xoffset=0; v.yoffset=0; v.bits_per_pixel=32;
    v.red.offset=16; v.red.length=8; v.green.offset=8; v.green.length=8; v.blue.offset=0; v.blue.length=8;
    v.pixclock=pixc;
    v.left_margin=1; v.right_margin=1; v.upper_margin=1; v.lower_margin=lo;
    v.hsync_len=1; v.vsync_len=1;
    v.sync=0; v.vmode=FB_VMODE_NONINTERLACED; v.activate=FB_ACTIVATE_NOW;
    int rc=ioctl(fb_fd,FBIOPUT_VSCREENINFO,&v);
    ioctl(fb_fd,FBIOGET_VSCREENINFO,&vi);
    return rc;
}
static int pan(uint32_t yoff){ vi.yoffset=yoff; return ioctl(fb_fd,FBIOPAN_DISPLAY,&vi); }

static void fill_first_line(uint32_t *l){
    for(int i=0;i<CELLS;i++) l[i]=0x00430000u;
    for(int i=20;i<143;i++) l[i]|=0x00040000u;
    for(int i=40;i<103;i++) l[i]&=0xFFFDFFFFu;
}
static void fill_line(uint32_t *l,int content){
    for(int i=0;i<CELLS;i++) l[i]=0x00410000u;
    for(int i=8;i<19;i++)   l[i]|=0x00200000u;
    for(int i=55;i<255;i++) l[i]|=0x00020000u;
    if(content) for(int i=26;i<260;i++) l[i]|=0x00100000u;
}
static void build_slot(unsigned char *buf){
    uint32_t *c=(uint32_t*)buf;
    fill_first_line(c+0*CELLS);
    fill_line(c+1*CELLS,0);
    fill_line(c+2*CELLS,0);
    fill_line(c+3*CELLS,1);
    for(int r=4;r<ROWS;r++){
        uint32_t *d=c+r*CELLS; const uint32_t *s2=c+3*CELLS;
        for(int i=0;i<CELLS;i++) d[i]=s2[i];      /* 32 位整字拷贝，不能用 memcpy */
    }
}
static void prep(const unsigned char *src,unsigned char T[256]){
    for(int gc=0;gc<16;gc++) for(int gp=0;gp<16;gp++){
        int idx=((gc*2)<<5)|(gp*2);
        T[(gc<<4)|gp]=(src[idx>>2]>>((idx&3)*2))&3;
    }
}
/* key 平面: kp[y*?] = (tgt<<4)|prv, 每列连续存放 (x-major, 与 img/prv 同布局) */
static unsigned char kp[UW*UH];
static void build_kp(void){
    size_t n=(size_t)UW*UH;
    for(size_t i=0;i<n;i++) kp[i]=(unsigned char)((img[i]<<4)|prv[i]);
}
/* 优化版: 单次表查找 + 展开 8 lane, 目标 <11.67ms/相位 */
static void encode_phase2(const unsigned char T[256], uint16_t *out){
    for(int pc=0;pc<COLS;pc++){
        uint16_t *o=out+(size_t)pc*CSTR;
        const unsigned char *k=kp+(size_t)(COLS-1-pc)*UH;
        for(int s=0;s<CSTR;s++){
            const unsigned char *b=k+(UH-1-s*8)-7;
            o[s]=(uint16_t)( (unsigned)T[b[0]]      | ((unsigned)T[b[1]]<<2)
                           | ((unsigned)T[b[2]]<<4) | ((unsigned)T[b[3]]<<6)
                           | ((unsigned)T[b[4]]<<8) | ((unsigned)T[b[5]]<<10)
                           | ((unsigned)T[b[6]]<<12)| ((unsigned)T[b[7]]<<14) );
        }
    }
}
/* 把一相位编码成 1404 x 234 的 u16 数组（按面板列优先） */
static void encode_phase(const unsigned char T[256], uint16_t *out){
    for(int pc=0;pc<COLS;pc++){
        uint16_t *o=out+(size_t)pc*CSTR;
        int ux=COLS-1-pc;
        const unsigned char *cp=img+(size_t)ux*UH, *pp=prv+(size_t)ux*UH;
        for(int s=0;s<CSTR;s++){
            int y0=UH-1-s*8; unsigned v=0;
            for(int lane=0;lane<8;lane++){
                unsigned t=cp[y0-lane]&0xF, p=pp[y0-lane]&0xF;
                v |= (unsigned)T[(t<<4)|p] << ((7-lane)*2);
            }
            o[s]=(uint16_t)v;
        }
    }
}
/* 把一相位写进槽 slotidx 并播放 */
static void play(const uint16_t *pd,int slotidx){
    unsigned char *buf=fbmem+(size_t)slotidx*SLOT;
    for(int pc=0;pc<COLS;pc++){
        uint32_t *l=(uint32_t*)(buf+(size_t)(4+pc)*LINE);
        const uint16_t *o=pd+(size_t)pc*CSTR;
        for(int s=0;s<CSTR;s++) l[26+s]=(l[26+s]&0xFFFF0000u)|o[s];
    }
    pan(slotidx*ROWS);
}
/* ---------------- 多温度档波形表 (panel/wbf_bands.bin) ----------------
   布局: "MXWF" | u16 ver | u8 nband | u8 nmode | u16 npool
         u16 bounds[nband+1] | u16 nph[nmode*nband]
         u16 rows[sum(nph)] | u8 pool[npool][256]
   模式顺序: 0=DU 1=GC16 2=A2 3=DU4                                     */
static unsigned char *wbf_blob;
static int wbf_nband,wbf_nmode,wbf_npool;
static const unsigned short *wbf_bounds,*wbf_nph,*wbf_rows;
static const unsigned char *wbf_pool;
static unsigned short rd16(const unsigned char*p){return (unsigned short)(p[0]|(p[1]<<8));}
static int wbf_load(const char*path){
    int fd=open(path,O_RDONLY|O_CLOEXEC); if(fd<0) return -1;
    off_t sz=lseek(fd,0,SEEK_END); lseek(fd,0,SEEK_SET);
    if(sz<64||sz>(16<<20)){close(fd);return -2;}
    wbf_blob=malloc((size_t)sz);
    if(!wbf_blob){close(fd);return -3;}
    if(read(fd,wbf_blob,(size_t)sz)!=(ssize_t)sz){close(fd);return -4;}
    close(fd);
    if(memcmp(wbf_blob,"MXWF",4)) return -5;
    size_t o=4, n=0;
    wbf_nband=wbf_blob[o+2]; wbf_nmode=wbf_blob[o+3]; wbf_npool=rd16(wbf_blob+o+4); o+=6;
    n=(size_t)(wbf_nband+1)*2; wbf_bounds=(const unsigned short*)(wbf_blob+o); o+=n;
    /* nph 区 = nmode*nband 个 u16; rows 区 = sum(nph) 个 u16 —— 两者长度不同! */
    wbf_nph=(const unsigned short*)(wbf_blob+o);
    o+=(size_t)wbf_nmode*wbf_nband*2;
    size_t tot=0;
    for(int m=0;m<wbf_nmode;m++) for(int b=0;b<wbf_nband;b++) tot+=wbf_nph[(size_t)m*wbf_nband+b];
    wbf_rows=(const unsigned short*)(wbf_blob+o); o+=tot*2;
    wbf_pool=wbf_blob+o;
    if(o+(size_t)wbf_npool*256!=(size_t)sz){ printf("wbf_blob 布局校验失败: %zu vs %zu\n",o+(size_t)wbf_npool*256,(size_t)sz); return -6; }
    return 0;
}
static int wbf_band_for(int temp_c){
    int b=0; for(int j=0;j<wbf_nband;j++) if(temp_c>=(int)wbf_bounds[j]) b=j;
    return b;
}
static const unsigned char* wbf_phase(int mode,int band,int ph,int*nph){
    int base=mode*wbf_nband+band, n=wbf_nph[base];
    if(ph<0||ph>=n) return 0;
    size_t start=0;
    for(int i=0;i<base;i++) start+=wbf_nph[i];
    *nph=n;
    return wbf_pool+(size_t)wbf_rows[start+ph]*256;
}
/* 统一取相位: 优先 blob, 否则退回编译进二进制的 band7 表 */
static const unsigned char* get_phase(int mode,int band,int ph,int*nph){
    if(wbf_pool) return wbf_phase(mode,band,ph,nph);
    if(band!=7) return 0;
    switch(mode){
      case 0: *nph=NPH_DU;   return (ph<NPH_DU)?lut_DU[ph]:0;
      case 1: *nph=NPH_GC16; return (ph<NPH_GC16)?lut_GC16[ph]:0;
      case 2: *nph=NPH_A2;   return (ph<NPH_A2)?lut_A2[ph]:0;
      default:*nph=NPH_DU4;  return (ph<NPH_DU4)?lut_DU4[ph]:0;
    }
}
/* ---------------- 快速整槽模板(纯 32 位写, 1.9ms/槽) ---------------- */
static uint32_t *slab;
static void slab_init(void){
    if(slab) return;
    slab=malloc(SLOT);
    fill_first_line(slab+0*CELLS);
    fill_line(slab+1*CELLS,0);
    fill_line(slab+2*CELLS,0);
    fill_line(slab+3*CELLS,1);
    for(int r=4;r<ROWS;r++) for(int i=0;i<CELLS;i++) slab[(size_t)r*CELLS+i]=slab[3*CELLS+i];
}
static void slab_put(const uint16_t *one){
    for(int pc=0;pc<COLS;pc++){
        uint32_t *l=slab+(size_t)(4+pc)*CELLS;
        const uint16_t *o=one+(size_t)pc*CSTR;
        for(int s=0;s<CSTR;s++) l[26+s]=(l[26+s]&0xFFFF0000u)|o[s];
    }
}
static void slab_put_col(int pc,const uint16_t *o){
    uint32_t *l=slab+(size_t)(4+pc)*CELLS;
    for(int s=0;s<CSTR;s++) l[26+s]=(l[26+s]&0xFFFF0000u)|o[s];
}
static void slab_zero_col(int pc){
    uint32_t *l=slab+(size_t)(4+pc)*CELLS;
    for(int s=0;s<CSTR;s++) l[26+s]&=0xFFFF0000u;
}
static void slab_zero_all(void){
    for(int pc=0;pc<COLS;pc++) slab_zero_col(pc);
}
static void slab_flush(int slotIdx){
    const uint32_t *s=slab;
    uint32_t *d=(uint32_t*)(fbmem+(size_t)slotIdx*SLOT);
    for(size_t i=0;i<(size_t)SLOT/4;i++) d[i]=s[i];
}
static void show_blank(void){
    for(int pc=0;pc<COLS;pc++) slab_zero_col(pc);
    slab_flush(0);
    pan(0);
}
/* 显示一帧并保持 hold 个帧周期(全部 pan 同一槽, 所以只写槽 0) */
static void show(const uint16_t *one,int hold){
    slab_put(one); slab_flush(0);
    for(int i=0;i<hold;i++) pan(0);
}
/* ---------------- LCDIF 寄存器直读 ---------------- */
#define LCDIF_PHYS 0x30730000u
static volatile uint32_t *lcd;
static int lcd_map(void){
    int fd=open("/dev/mem",O_RDONLY|O_SYNC);
    if(fd<0){perror("dev/mem");return -1;}
    void *p=mmap(0,0x1000,PROT_READ,MAP_SHARED,fd,LCDIF_PHYS);
    close(fd);
    if(p==MAP_FAILED){perror("mmap lcdif");return -1;}
    lcd=(volatile uint32_t*)p; return 0;
}
static uint32_t LR(int off){ return lcd?lcd[off/4]:0xDEADBEEFu; }
static void dump_lcd(const char*tag){
    /* 实测布局: 每个寄存器占 0x10, +4/+8/+12 是 SET/CLR/TOG 别名 */
    printf("  %-9s CTRL=%08x CTRL1=%08x CTRL2=%08x TC=%08x CUR=%08x NEXT=%08x TIM=%08x VD0=%08x VD1=%08x VD2=%08x VD4=%08x\n",
        tag, LR(0x00),LR(0x10),LR(0x20),LR(0x30),LR(0x40),LR(0x50),LR(0x60),LR(0x70),LR(0x80),LR(0x90),LR(0xB0));
}
/* -------- 事件时序: pan 调用 vs CUR_BUF 实际改变 -------- */
#include <pthread.h>
static volatile int poll_run;
#define NEV 400
static double ev_t[NEV]; static int ev_k[NEV]; static uint32_t ev_v[NEV]; static int nev;
static void ev_add(int k,uint32_t v){
    if(nev<NEV){ ev_t[nev]=now(); ev_k[nev]=k; ev_v[nev]=v; nev++; }
}
static void* poller(void*arg){
    uint32_t last=LR(0x40);
    while(poll_run){
        uint32_t v=LR(0x40);
        if(v!=last){ ev_add(2,v); last=v; }
    }
    return 0;
}
static void fill_code(unsigned char *buf,uint16_t code){
    for(int r=4;r<ROWS;r++){ uint32_t *l=(uint32_t*)(buf+(size_t)r*LINE);
        for(int c=0;c<CELLS;c++) l[c]=(l[c]&0xFFFF0000u)|code; }
}
static void make_page(void){
    for(int x=0;x<UW;x++) memset(img+(size_t)x*UH,15,UH);
    for(int y=30;y<150;y++) for(int x=0;x<UW;x++) img[(size_t)x*UH+y]=0;
    for(int k=0;k<6;k++){ int x0=80+k*210;
        for(int y=70;y<110;y++) for(int x=x0;x<x0+140;x++) img[(size_t)x*UH+y]=15; }
    for(int r=0;r<22;r++){ int y=220+r*56; int w=(r%5==0)?900:((r%3==0)?760:1000);
        for(int yy=y;yy<y+14;yy++) for(int x=90;x<90+w;x++) img[(size_t)x*UH+yy]=0; }
    for(int g=0;g<16;g++){ int y=1560;
        for(int yy=y;yy<y+180;yy++) for(int x=g*80+20;x<g*80+80;x++) img[(size_t)x*UH+yy]=(unsigned char)g; }
}
int main(int argc,char**argv){
    const char *m=argc>1?argv[1]:"g16";
    int limit=argc>2?atoi(argv[2]):0;
    fb_fd=open("/dev/fb0",O_RDWR); if(fb_fd<0){perror("fb0");return 1;}
    if(ioctl(fb_fd,FBIOGET_VSCREENINFO,&vi)){perror("getvar");return 1;}
    fbmem=mmap(0,32UL<<20,PROT_READ|PROT_WRITE,MAP_SHARED,fb_fd,0);
    if(fbmem==MAP_FAILED){perror("mmap");return 1;}
    { const char *cand[]={"/tmp/wbf_bands.bin","/opt/wave/lib/wbf_bands.bin",
                          "./wbf_bands.bin","/usr/share/remarkable/wbf_bands.bin",0};
      for(int i=0;cand[i];i++){ int r=wbf_load(cand[i]);
        if(r==0){ printf("波形表: %s (%d 档 x %d 模式, %d 个去重相位)\n",
                         cand[i],wbf_nband,wbf_nmode,wbf_npool); break; }
        if(r!=-1&&r!=-2&&r!=-3){ printf("波形表 %s 载入失败 %d\n",cand[i],r); break; } } }
    if(setmode(CELLS,ROWS,NSLOT*ROWS,0x7080,143)){perror("setmode");return 1;}
    printf("mode: xres=%u yres=%u yvirt=%u pixclock=%u margins=%u/%u/%u/%u\n",
           vi.xres,vi.yres,vi.yres_virtual,vi.pixclock,vi.left_margin,vi.right_margin,vi.upper_margin,vi.lower_margin);
    for(int s=0;s<NSLOT;s++) build_slot(fbmem+(size_t)s*SLOT);
    /* 量一下 pan() 本身是否阻塞 */
    { double a=now(); for(int i=0;i<20;i++) pan((i&1)*ROWS); double b=now();
      printf("pan() 单独 20 次: %.1f ms (%.2f ms/次)  <- 若约等于帧周期则阻塞\n",b-a,(b-a)/20); }

    if(!strcmp(m,"half")){
        unsigned char *buf=fbmem+0*SLOT;
        for(int pc=0;pc<COLS;pc++){
            uint32_t *l=(uint32_t*)(buf+(size_t)(4+pc)*LINE);
            uint16_t code = (pc<702)?0x5555u:0xAAAAu;
            for(int c=0;c<CELLS;c++) l[c]=(l[c]&0xFFFF0000u)|code;
        }
        double a=now();
        for(int i=0;i<24;i++) pan(0);
        printf("half 24 帧: %.1f ms\n",now()-a);
        return 0;
    }
    if(!strcmp(m,"stripe")){
        /* 只把 cell 26..233 推黑 (208 cell = 1664 px)，留白边判偏移 */
        unsigned char *buf=fbmem+0*SLOT;
        for(int pc=0;pc<COLS;pc++){
            uint32_t *l=(uint32_t*)(buf+(size_t)(4+pc)*LINE);
            for(int c=26;c<=233;c++) l[c]=(l[c]&0xFFFF0000u)|0x5555u;
        }
        for(int i=0;i<30;i++) pan(0);
        printf("stripe: cell26..233 推黑 30 帧，完成\n");
        return 0;
    }
    if(!strcmp(m,"bw")){
        /* 平凡 LUT: tgt<8 -> code1(黑), 否则 code2(白)，重复 24 帧，走真实 encode+play 路径 */
        make_page(); memset(prv,15,sizeof prv);
        uint16_t *one=malloc((size_t)COLS*CSTR*2);
        for(int pc=0;pc<COLS;pc++){
            int ux=COLS-1-pc; const unsigned char *cp=img+(size_t)ux*UH;
            for(int s=0;s<CSTR;s++){ int y0=UH-1-s*8; unsigned v=0;
                for(int lane=0;lane<8;lane++)
                    v |= ((cp[y0-lane]<8)?1u:2u) << ((7-lane)*2);
                one[(size_t)pc*CSTR+s]=(uint16_t)v; }
        }
        long c1=0,c2=0,c0=0;
        for(int i=0;i<COLS*CSTR;i++) for(int l=0;l<8;l++){int c=(one[i]>>(l*2))&3; if(c==1)c1++; else if(c==2)c2++; else if(c==0)c0++;}
        printf("bw codes: 0=%ld 1=%ld 2=%ld\n",c0,c1,c2);
        printf("bw one[0]=%04x one[10000]=%04x\n",one[0],one[10000]);
        play(one,0);
        { uint32_t *r=(uint32_t*)(fbmem+0*SLOT+(size_t)4*LINE);
          printf("bw fb row4 cell26=%08x cell27=%08x (期望低16位 %04x %04x)\n",
                 r[26],r[27],one[0],one[1]); }
        for(int i=0;i<30;i++) pan(0);
        printf("bw: slot0 30 帧完成\n");
        return 0;
    }
    if(!strcmp(m,"init")){
        /* 官方 zplash INIT 字节序列：以 code2 结尾，正常应停在白 */
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        int nf=0;
        double a=now();
        for(int k=0;k<16;k+=2){
            int code=seq[k], cnt=seq[k+1];
            uint16_t v = (code==0)?0u:((code==1)?0x5555u:0xAAAAu);
            unsigned char *b0=fbmem+0*SLOT, *b1=fbmem+1*SLOT;
            for(int r=4;r<ROWS;r++){ uint32_t*l0=(uint32_t*)(b0+(size_t)r*LINE);
                                     uint32_t*l1=(uint32_t*)(b1+(size_t)r*LINE);
                for(int c=0;c<CELLS;c++){ uint32_t m0=(l0[c]&0xFFFF0000u)|v, m1=(l1[c]&0xFFFF0000u)|v;
                                          l0[c]=m0; l1[c]=m1; } }
            for(int i=0;i<cnt;i++){ pan((i&1)*ROWS); nf++; }
        }
        printf("init: %d 帧, %.1f ms, 末尾 code2 -> 期望白色\n",nf,now()-a);
        return 0;
    }
    if(!strcmp(m,"all")){
        /* 把 bw 画面写进全部 17 槽，再轮着 pan —— 排除槽选择问题 */
        make_page();
        static uint16_t one[COLS*CSTR];
        for(int pc=0;pc<COLS;pc++){
            int ux=COLS-1-pc; const unsigned char *cp=img+(size_t)ux*UH;
            for(int s2=0;s2<CSTR;s2++){ int y0=UH-1-s2*8; unsigned v=0;
                for(int lane=0;lane<8;lane++)
                    v |= ((cp[y0-lane]<8)?1u:2u) << ((7-lane)*2);
                one[(size_t)pc*CSTR+s2]=(uint16_t)v; }
        }
        for(int sl=0;sl<NSLOT;sl++){
            unsigned char *buf=fbmem+(size_t)sl*SLOT;
            for(int pc=0;pc<COLS;pc++){
                uint32_t *l=(uint32_t*)(buf+(size_t)(4+pc)*LINE);
                const uint16_t *o=one+(size_t)pc*CSTR;
                for(int s3=0;s3<CSTR;s3++) l[26+s3]=(l[26+s3]&0xFFFF0000u)|o[s3];
            }
        }
        int er=0; double a=now();
        for(int r=0;r<3;r++) for(int sl=0;sl<NSLOT;sl++){
            vi.yoffset=(uint32_t)(sl*ROWS);
            int rc=ioctl(fb_fd,FBIOPAN_DISPLAY,&vi);
            if(rc<0) er++;
        }
        printf("all: 17 槽 x3 轮, 51 次 pan, 失败 %d 次, %.1f ms\n",er,now()-a);
        return 0;
    }
    if(!strcmp(m,"mix")){
        /* 同一行内混码: px 0..935 -> code1(黑), px 936..1871 -> code2(白)
           期望: 下半黑, 上半白 */
        unsigned char *buf=fbmem+0*SLOT, *buf1=fbmem+1*SLOT;
        for(int pc=0;pc<COLS;pc++){
            uint32_t *l =(uint32_t*)(buf +(size_t)(4+pc)*LINE);
            uint32_t *l1=(uint32_t*)(buf1+(size_t)(4+pc)*LINE);
            for(int c=26;c<260;c++){
                uint16_t code = (c<143)?0x5555u:0xAAAAu;
                l[c] =(l[c] &0xFFFF0000u)|code;
                l1[c]=(l1[c]&0xFFFF0000u)|code;
            }
        }
        for(int i=0;i<30;i++) pan((i&1)*ROWS);
        printf("mix: 行内混码 30 帧完成\n");
        return 0;
    }
    if(!strcmp(m,"page")){
        /* 两段式: 1) 官方 INIT 建立白底  2) 按图像把暗像素推黑(code1)，其余 code0 */
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        double a=now();
        for(int k=0;k<16;k+=2){
            int code=seq[k],cnt=seq[k+1];
            uint16_t v=(code==0)?0u:((code==1)?0x5555u:0xAAAAu);
            for(int sl=0;sl<2;sl++){ unsigned char *b=fbmem+(size_t)sl*SLOT;
                for(int r=4;r<ROWS;r++){ uint32_t*l=(uint32_t*)(b+(size_t)r*LINE);
                    for(int c=0;c<CELLS;c++) l[c]=(l[c]&0xFFFF0000u)|v; } }
            for(int i=0;i<cnt;i++) pan((i&1)*ROWS);
        }
        printf("page: INIT %d 帧 %.0f ms\n",111,now()-a);
        /* 第二段: 只推黑暗像素 */
        make_page();
        static uint16_t one[COLS*CSTR];
        long nd=0;
        for(int pc=0;pc<COLS;pc++){
            int ux=COLS-1-pc; const unsigned char *cp=img+(size_t)ux*UH;
            for(int s2=0;s2<CSTR;s2++){ int y0=UH-1-s2*8; unsigned v=0;
                for(int lane=0;lane<8;lane++) if(cp[y0-lane]<8){ v|=1u<<((7-lane)*2); nd++; }
                one[(size_t)pc*CSTR+s2]=(uint16_t)v; }
        }
        for(int sl=0;sl<2;sl++){ unsigned char *b=fbmem+(size_t)sl*SLOT;
            for(int pc=0;pc<COLS;pc++){
                uint32_t *l=(uint32_t*)(b+(size_t)(4+pc)*LINE);
                const uint16_t *o=one+(size_t)pc*CSTR;
                for(int s2=0;s2<CSTR;s2++) l[26+s2]=(l[26+s2]&0xFFFF0000u)|o[s2];
            } }
        a=now(); for(int i=0;i<40;i++) pan((i&1)*ROWS);
        printf("page: 推黑 %ld 个像素, 40 帧 %.0f ms\n",nd,now()-a);
        return 0;
    }
    if(!strcmp(m,"ramp")){
        /* 纯脉冲计数灰阶：INIT 刷白后，第 k 帧只推黑那些 t <= 15-k 的像素。
           目标灰阶 t 得到 (15-t) 帧 code1 -> 灰阶 = 15 - 帧数，线性。
           全程 15 帧，不需要波形表。 */
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        double a=now();
        for(int k=0;k<16;k+=2){
            int code=seq[k],cnt=seq[k+1];
            uint16_t v=(code==0)?0u:((code==1)?0x5555u:0xAAAAu);
            for(int sl=0;sl<2;sl++){ unsigned char *b=fbmem+(size_t)sl*SLOT;
                for(int r=4;r<ROWS;r++){ uint32_t*l=(uint32_t*)(b+(size_t)r*LINE);
                    for(int c=0;c<CELLS;c++) l[c]=(l[c]&0xFFFF0000u)|v; } }
            for(int i=0;i<cnt;i++) pan((i&1)*ROWS);
        }
        printf("ramp: INIT 刷白 %.0f ms\n",now()-a);
        make_page();
        static uint32_t slot[ROWS*CELLS/4*0 + ROWS*0];  /* 占位，避免空声明 */
        static uint32_t buf[ROWS*CELLS];                /* 1.46 MB RAM 槽 */
        fill_first_line(buf+0*CELLS);
        fill_line(buf+1*CELLS,0);
        fill_line(buf+2*CELLS,0);
        fill_line(buf+3*CELLS,1);
        for(int r=4;r<ROWS;r++)
            for(int i=0;i<CELLS;i++) buf[(size_t)r*CELLS+i]=buf[3*CELLS+i];
        a=now();
        for(int k=1;k<=30;k++){
            int lim=15-((k+1)>>1);
            for(int pc=0;pc<COLS;pc++){
                int ux=COLS-1-pc; const unsigned char *cp=img+(size_t)ux*UH;
                uint32_t *l=buf+(size_t)(4+pc)*CELLS;
                for(int s2=0;s2<CSTR;s2++){ int y0=UH-1-s2*8; unsigned v=0;
                    for(int lane=0;lane<8;lane++) if(cp[y0-lane]<=lim) v|=1u<<((7-lane)*2);
                    l[26+s2]=(l[26+s2]&0xFFFF0000u)|v; }
            }
            int sl=k&1; unsigned char *dst=fbmem+(size_t)sl*SLOT;
            uint32_t *d=(uint32_t*)dst;
            for(size_t i=0;i<(size_t)SLOT/4;i++) d[i]=buf[i];
            pan(sl*ROWS);
        }
        printf("ramp: 30 帧灰阶 %.0f ms (%.1f ms/帧)\n",now()-a,(now()-a)/30);
        return 0;
    }

    if(!strcmp(m,"regs")){
        /* 只读寄存器: pan 到不同地址时 CUR_BUF 是否真的变 */
        if(lcd_map()){printf("no /dev/mem\n");return 1;}
        dump_lcd("before");
        pan(0);            dump_lcd("pan(0)");
        pan(1*ROWS);       dump_lcd("pan(1408)");
        pan(2*ROWS);       dump_lcd("pan(2816)");
        pan(2*ROWS);       dump_lcd("pan(same)");
        double a=now(); for(int i=0;i<20;i++) pan(2*ROWS);       double b=now();
        printf("  pan(同一槽) 20 次: %.2f ms/次\n",(b-a)/20);
        a=now(); for(int i=0;i<20;i++) pan((i&1)*ROWS);          b=now();
        printf("  pan(槽0/1 交替) 20 次: %.2f ms/次\n",(b-a)/20);
        a=now(); for(int i=0;i<20;i++) pan(2*ROWS);              b=now();
        printf("  pan(同一槽) 再 20 次: %.2f ms/次\n",(b-a)/20);
        for(int off=0;off<0x200;off+=0x10)
            printf("   %03x: %08x %08x %08x %08x\n",off,LR(off),LR(off+4),LR(off+8),LR(off+12));
        return 0;
    }
    if(!strcmp(m,"swap")){
        /* 决定性测试: 槽0 全 code1(推黑), 槽1 全 code2(推白)
           期望 20 帧黑 -> 40 帧白。若最终仍是全黑, 说明 pan() 没生效。 */
        if(lcd_map()) printf("  (无 /dev/mem, 只看画面)\n");
        fill_code(fbmem+0*SLOT,0x5555u);
        fill_code(fbmem+1*SLOT,0xAAAAu);
        printf("swap: slot0=推黑 slot1=推白\n"); dump_lcd("init");
        for(int i=0;i<20;i++) pan(0);
        printf("swap: 槽0 已 20 帧 (期望全黑)\n"); dump_lcd("after0");
        for(int i=0;i<40;i++) pan(1*ROWS);
        printf("swap: 槽1 已 40 帧 (若 pan 生效 -> 全白; 仍全黑 -> pan 未生效)\n"); dump_lcd("after1");
        return 0;
    }

    if(!strcmp(m,"order")){
        /* 量: pan() 里 CUR_BUF 是在调用开始时改, 还是等 vsync 时才改 */
        if(lcd_map()){printf("no /dev/mem\n");return 1;}
        pthread_t th; nev=0; poll_run=1;
        if(pthread_create(&th,0,poller,0)){perror("thread");return 1;}
        usleep(30000);
        for(int i=0;i<6;i++){
            ev_add(0,(uint32_t)i);
            pan((i&1)*ROWS);
            ev_add(1,(uint32_t)i);
            usleep(3000);
        }
        poll_run=0; pthread_join(th,0);
        for(int i=0;i<nev;i++) for(int j=i+1;j<nev;j++)
            if(ev_t[j]<ev_t[i]){ double t=ev_t[i];ev_t[i]=ev_t[j];ev_t[j]=t;
                int k=ev_k[i];ev_k[i]=ev_k[j];ev_k[j]=k;
                uint32_t v=ev_v[i];ev_v[i]=ev_v[j];ev_v[j]=v; }
        double b0=ev_t[0];
        printf("事件时间线 (0=pan开始 1=pan返回 2=CUR_BUF改变):\n");
        for(int i=0;i<nev;i++){
            if(ev_k[i]==2) printf("  +%8.3f ms   CUR_BUF -> %08x\n",ev_t[i]-b0,ev_v[i]);
            else printf("  +%8.3f ms   %s [%u]\n",ev_t[i]-b0,
                        ev_k[i]?"pan 返回":"pan 调用",ev_v[i]);
        }
        double a=now();
        for(int pc=0;pc<COLS;pc++){ uint32_t *l=(uint32_t*)(fbmem+0*SLOT+(size_t)(4+pc)*LINE);
            for(int s2=0;s2<CSTR;s2++) l[26+s2]=(l[26+s2]&0xFFFF0000u)|0x5555u; }
        printf("写满一个槽 (%dx%d cell): %.2f ms\n",COLS,CSTR,now()-a);
        return 0;
    }
    if(!strcmp(m,"nw")){
        /* 决定性: 用 /dev/mem(非 write-combining) 回读, 看 WC 是否真的丢数据 */
        struct fb_fix_screeninfo fi; memset(&fi,0,sizeof fi);
        if(ioctl(fb_fd,FBIOGET_FSCREENINFO,&fi)){perror("getfix");return 1;}
        printf("fix: smem_start=%08lx smem_len=%u line_len=%u accel=%u\n",
               (unsigned long)fi.smem_start,fi.smem_len,fi.line_length,fi.accel);
        int md=open("/dev/mem",O_RDWR|O_SYNC); if(md<0){perror("mem");return 1;}
        size_t mlen=(size_t)NSLOT*SLOT;
        unsigned char *fbx=mmap(0,mlen,PROT_READ|PROT_WRITE,MAP_SHARED,md,(off_t)fi.smem_start);
        if(fbx==MAP_FAILED){perror("mmap fbx");return 1;}
        printf("fbx 映射成功 %zu 字节, 与 fbmem 的差值 = %ld\n",mlen,
               (long)((unsigned char*)fbmem-(unsigned char*)fbx));
        for(int pass=0;pass<3;pass++){
            uint16_t v = pass==0?0x5555u : (pass==1?0xAAAAu:0x1234u);
            double a=now();
            for(int r=4;r<ROWS;r++){ uint32_t*l=(uint32_t*)(fbmem+(size_t)r*LINE);
                for(int c=26;c<CELLS;c++) l[c]=(l[c]&0xFFFF0000u)|v; }
            double b=now();
            long bad=0; uint32_t first=0; int fr=-1,fc=-1;
            for(int r=4;r<ROWS;r++){
                const uint32_t*s=(const uint32_t*)(fbx+(size_t)r*LINE);
                for(int c=26;c<CELLS;c++)
                    if((s[c]&0xFFFFu)!=v){ if(!bad){first=s[c];fr=r;fc=c;} bad++; }
            }
            printf("pass%d v=%04x 写 %.2f ms, 经 /dev/mem 回读不符 %ld 格 (首个 r=%d c=%d : %08x)\n",
                   pass,v,b-a,bad,fr,fc,first);
        }
        /* 纯字拷贝(无读-改-写)的写速度 */
        static uint32_t buf[ROWS*CELLS];
        for(int r=0;r<ROWS*CELLS;r++) buf[r]=0x41A00000u|0x5555u;
        double a=now();
        for(int sl=0;sl<2;sl++){ uint32_t*d=(uint32_t*)(fbmem+(size_t)sl*SLOT);
            for(size_t i=0;i<(size_t)SLOT/4;i++) d[i]=buf[i]; }
        printf("整槽 32 位纯写 x2: %.2f ms (%.2f ms/槽)\n",now()-a,(now()-a)/2);
        a=now();
        for(int sl=0;sl<2;sl++){ uint32_t*d=(uint32_t*)(fbx+(size_t)sl*SLOT);
            for(size_t i=0;i<(size_t)SLOT/4;i++) d[i]=buf[i]; }
        printf("整槽 32 位纯写 x2 经 /dev/mem: %.2f ms (%.2f ms/槽)\n",now()-a,(now()-a)/2);
        return 0;
    }
    if(!strcmp(m,"quad")){
        /* 一段一次只推黑一个象限, 共 4 段 x 24 帧, 两槽写一样的内容。
           若"换帧"正常 -> 四个象限全黑;
           若只有第一段生效 -> 只有左上黑;
           若只有最后一段生效 -> 只有右下黑。 */
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        for(int k=0;k<16;k+=2){
            int code=seq[k],cnt=seq[k+1];
            uint16_t v=(code==0)?0u:((code==1)?0x5555u:0xAAAAu);
            for(int sl=0;sl<2;sl++) fill_code(fbmem+(size_t)sl*SLOT,v);
            for(int i=0;i<cnt;i++) pan((i&1)*ROWS);
        }
        printf("quad: INIT 完成(白)\n");
        static uint16_t one[COLS*CSTR];
        const char*nm[4]={"左上","右上","左下","右下"};
        for(int q=0;q<4;q++){
            int qx=q&1, qy=q>>1;
            long nd=0;
            for(int pc=0;pc<COLS;pc++){
                int ux=COLS-1-pc;
                for(int s2=0;s2<CSTR;s2++){
                    int y0=UH-1-s2*8; unsigned v=0;
                    for(int lane=0;lane<8;lane++){
                        int y=y0-lane;
                        if(((ux>=702)?1:0)==qx && ((y>=936)?1:0)==qy){ v|=1u<<((7-lane)*2); nd++; }
                    }
                    one[(size_t)pc*CSTR+s2]=(uint16_t)v;
                }
            }
            for(int sl=0;sl<2;sl++){ unsigned char *b=fbmem+(size_t)sl*SLOT;
                for(int pc=0;pc<COLS;pc++){
                    uint32_t *l=(uint32_t*)(b+(size_t)(4+pc)*LINE);
                    const uint16_t *o=one+(size_t)pc*CSTR;
                    for(int s2=0;s2<CSTR;s2++) l[26+s2]=(l[26+s2]&0xFFFF0000u)|o[s2];
                } }
            for(int i=0;i<24;i++) pan((i&1)*ROWS);
            printf("quad: 第 %d 段 (%s) %ld 像素推黑 x24 帧\n",q+1,nm[q],nd);
        }
        printf("quad: 完成。期望四象限全黑\n");
        return 0;
    }
    if(!strcmp(m,"ladder")){
        /* 8 条竖条, 分别给 N 帧纯 code1 推黑 -> 一条灰度响应曲线 */
        static const int NF[8]={1,2,3,4,6,8,12,20};
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        for(int k=0;k<16;k+=2){
            int code=seq[k],cnt=seq[k+1];
            uint16_t v=(code==0)?0u:((code==1)?0x5555u:0xAAAAu);
            for(int sl=0;sl<2;sl++) fill_code(fbmem+(size_t)sl*SLOT,v);
            for(int i=0;i<cnt;i++) pan((i&1)*ROWS);
        }
        printf("ladder: INIT 白底完成\n");
        static uint16_t one[COLS*CSTR];
        for(int st=0;st<8;st++){
            long np=0;
            for(int pc=0;pc<COLS;pc++){
                int ux=COLS-1-pc;
                int si=ux/175; if(si>7) si=7;
                int on=(si==st);
                for(int s2=0;s2<CSTR;s2++){
                    int y0=UH-1-s2*8; unsigned v=0;
                    if(on) for(int lane=0;lane<8;lane++) v|=1u<<((7-lane)*2);
                    one[(size_t)pc*CSTR+s2]=(uint16_t)v;
                    if(on) np+=8;
                }
            }
            for(int sl=0;sl<2;sl++){ unsigned char *b=fbmem+(size_t)sl*SLOT;
                for(int pc=0;pc<COLS;pc++){
                    uint32_t *l=(uint32_t*)(b+(size_t)(4+pc)*LINE);
                    const uint16_t *o=one+(size_t)pc*CSTR;
                    for(int s2=0;s2<CSTR;s2++) l[26+s2]=(l[26+s2]&0xFFFF0000u)|o[s2];
                } }
            for(int i=0;i<NF[st];i++) pan((i&1)*ROWS);
            printf("ladder: 条%d (x %d..%d) %d 帧 %ld 像素\n",st,st*175,(st+1)*175-1,NF[st],np);
        }
        printf("ladder: 完成。期望左浅右深\n");
        return 0;
    }
    if(!strcmp(m,"conv")){
        /* 4 条横带各用一种 LUT 索引约定, 每带画同一个黑白棋盘。
           哪条带出现清晰棋盘, 那种约定就是对的。
           band0: idx=tgt*32+prev 相位正序 (当前实现)
           band1: idx=prev*32+tgt 相位正序
           band2: idx=tgt*32+prev 相位倒序
           band3: idx=prev*32+tgt 相位倒序                     */
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        for(int k=0;k<16;k+=2){
            int code=seq[k],cnt=seq[k+1];
            uint16_t v=(code==0)?0u:((code==1)?0x5555u:0xAAAAu);
            for(int sl=0;sl<2;sl++) fill_code(fbmem+(size_t)sl*SLOT,v);
            for(int i=0;i<cnt;i++) pan((i&1)*ROWS);
        }
        printf("conv: INIT 白底完成\n");
        static unsigned char cw[4][46][2];
        for(int c=0;c<4;c++){
            int rev=c>>1, trans=c&1;
            for(int ph=0;ph<46;ph++){
                int pp=rev?(45-ph):ph;
                for(int t=0;t<2;t++){
                    int tg=t?15:0;
                    int idx = trans ? (30*32+2*tg) : (2*tg*32+30);
                    cw[c][ph][t]=(unsigned char)((lut_GC16[pp][idx>>2]>>((idx&3)*2))&3);
                }
            }
        }
        for(int c=0;c<4;c++){
            printf("conv: band%d p0 黑像素码=%d 白像素码=%d | p23 黑=%d 白=%d\n",
                c,cw[c][0][0],cw[c][0][1],cw[c][23][0],cw[c][23][1]);
        }
        static uint16_t pd[COLS*CSTR];
        for(int ph=0;ph<46;ph++){
            long n1=0,n2=0;
            for(int pc=0;pc<COLS;pc++){
                int ux=COLS-1-pc;
                for(int s2=0;s2<CSTR;s2++){
                    int y0=UH-1-s2*8; unsigned v=0;
                    for(int lane=0;lane<8;lane++){
                        int y=y0-lane; int band=y/468; if(band>3) band=3;
                        int ly=y-band*468;
                        int black=(((ux>=702)?1:0)^((ly>=234)?1:0));
                        int c=(unsigned char)cw[band][ph][black?0:1];
                        v|=(unsigned)c<<((7-lane)*2);
                        if(c==1)n1++; else if(c==2)n2++;
                    }
                    pd[(size_t)pc*CSTR+s2]=(uint16_t)v;
                }
            }
            for(int sl=0;sl<2;sl++){ unsigned char *b=fbmem+(size_t)sl*SLOT;
                for(int pc=0;pc<COLS;pc++){
                    uint32_t *l=(uint32_t*)(b+(size_t)(4+pc)*LINE);
                    const uint16_t *o=pd+(size_t)pc*CSTR;
                    for(int s2=0;s2<CSTR;s2++) l[26+s2]=(l[26+s2]&0xFFFF0000u)|o[s2];
                } }
            pan((ph&1)*ROWS);
            if(ph%10==0) printf("conv: ph%02d code1=%ld code2=%ld\n",ph,n1,n2);
        }
        printf("conv: 完成。找出哪条带显示清晰黑白棋盘\n");
        return 0;
    }
    if(!strcmp(m,"chk")){
        /* 对照: 同一个棋盘, 但只用纯 code1 推 24 帧(已知能变黑的方式) */
        slab_init();
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        static uint16_t one[COLS*CSTR];
        for(int k=0;k<16;k+=2){
            int c=seq[k],cnt=seq[k+1];
            uint16_t v=(c==0)?0u:((c==1)?0x5555u:0xAAAAu);
            for(int i=0;i<COLS*CSTR;i++) one[i]=v;
            show(one,cnt);
        }
        printf("chk: INIT 白底完成\n");
        long nd=0;
        for(int pc=0;pc<COLS;pc++){
            int ux=COLS-1-pc;
            for(int s2=0;s2<CSTR;s2++){
                int y0=UH-1-s2*8; unsigned v=0;
                for(int lane=0;lane<8;lane++){
                    int y=y0-lane; int band=y/468; if(band>3) band=3;
                    int ly=y-band*468;
                    int black=(((ux>=702)?1:0)^((ly>=234)?1:0));
                    if(black){ v|=1u<<((7-lane)*2); nd++; }
                }
                one[(size_t)pc*CSTR+s2]=(uint16_t)v;
            }
        }
        double a=now(); show(one,24);
        printf("chk: 棋盘 %ld 像素 x24 帧 code1, %.0f ms\n",nd,now()-a);
        printf("chk: 期望 每条横带都是黑白棋盘\n");
        return 0;
    }
    if(!strcmp(m,"du")||!strcmp(m,"a2")||!strcmp(m,"du4")||!strcmp(m,"g16")){
        int mi=1; const char *tag="GC16";
        if(m[0]=='d'&&m[1]=='u'&&m[2]=='4'){ mi=3; tag="DU4"; }
        else if(m[0]=='d'){ mi=0; tag="DU"; }
        else if(m[0]=='a'){ mi=2; tag="A2"; }
        int hold=argc>2?atoi(argv[2]):1; if(hold<1) hold=1;
        /* 温度: 未指定则读 /sys/class/thermal/thermal_zone0/temp */
        int tempc=argc>3?atoi(argv[3]):-1000;
        if(tempc<-100){
            int tf=open("/sys/class/thermal/thermal_zone0/temp",O_RDONLY);
            char tb[32]={0};
            if(tf>=0){ if(read(tf,tb,31)>0) tempc=atoi(tb)/1000;
                       close(tf); } else tempc=22;
        }
        int band=7;
        if(wbf_pool){ band=wbf_band_for(tempc); }
        int nph0=0;
        const unsigned char *first=get_phase(mi,band,0,&nph0);
        if(!first){ fprintf(stderr,"%s: 没有 band %d 的波形\n",tag,band); return 1; }
        int nph=nph0;
        printf("%s: 温度 %d C -> band %d (%d..%d C), %d 相位, 波形来源=%s\n",
               tag,tempc,band,(int)wbf_bounds[band],
               wbf_pool?(int)wbf_bounds[band+1]-1:23,nph,wbf_pool?"blob":"编译内置(仅band7)");
        slab_init(); make_page();
        static uint16_t one[COLS*CSTR];
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        double a=now();
        for(int k=0;k<16;k+=2){
            int c=seq[k],cnt=seq[k+1];
            uint16_t v=(c==0)?0u:((c==1)?0x5555u:0xAAAAu);
            for(int i=0;i<COLS*CSTR;i++) one[i]=v;
            show(one,cnt);
        }
        printf("%s: INIT %.0f ms\n",tag,now()-a);
        memset(prv,15,sizeof prv);
        build_kp();
        uint16_t *pd=malloc((size_t)nph*COLS*CSTR*2);
        unsigned char T[256];
        a=now();
        for(int ph=0;ph<nph;ph++){
            int nn=0; const unsigned char *row=get_phase(mi,band,ph,&nn);
            if(!row){ fprintf(stderr,"phase %d 缺失\n",ph); return 1; }
            prep(row,T); encode_phase2(T,pd+(size_t)ph*COLS*CSTR);
        }
        double te=now()-a;
        a=now();
        for(int ph=0;ph<nph;ph++) show(pd+(size_t)ph*COLS*CSTR,hold);
        double tp=now()-a;
        printf("%s: %d 相位, 编码 %.0f ms (%.2f ms/相位), 播放 %.0f ms (%.2f ms/相位, hold=%d)\n",
               tag,nph,te,te/nph,tp,tp/nph,hold);
        if(hold==1 && te/nph>11.0) printf("%s: 警告 编码超过一帧 %.1f ms, 波形会被拉长!\n",tag,te/nph);
        { /* 自检: 最后相位里 code1/code2 的像素数与位置 */
          long c1=0,c2=0; int minx=99999,maxx=-1,miny=99999,maxy=-1;
          const uint16_t *last=pd+(size_t)(nph-1)*COLS*CSTR;
          for(int pc=0;pc<COLS;pc++){ int ux=COLS-1-pc;
            for(int s=0;s<CSTR;s++){ unsigned v=last[(size_t)pc*CSTR+s];
              for(int lane=0;lane<8;lane++){ int c=(v>>((7-lane)*2))&3;
                if(c){ if(c==1)c1++; else c2++;
                  int y=UH-1-s*8-lane;
                  if(ux<minx)minx=ux; if(ux>maxx)maxx=ux;
                  if(y<miny)miny=y; if(y>maxy)maxy=y; } } } }
          printf("%s: 末相位 code1=%ld code2=%ld 范围 x %d..%d y %d..%d\n",
                 tag,c1,c2,minx,maxx,miny,maxy); }
        memcpy(prv,img,sizeof img);
        show_blank(); show_blank();
        printf("%s: 已补 2 帧全 0 无驱动帧\n",tag);
        return 0;
    }
    if(!strcmp(m,"pupd")){
        /* 局部刷新: 整屏 GC16 建立基线, 然后移动一个黑块只用 DU 推 NF 帧 */
        int NF = argc>2?atoi(argv[2]):8; if(NF<1) NF=1;
        int STEP = argc>3?atoi(argv[3]):110;
        int NSTEP = argc>4?atoi(argv[4]):10;
        slab_init(); make_page();
        static uint16_t one[COLS*CSTR];
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        for(int k2=0;k2<16;k2+=2){
            int c=seq[k2],cnt=seq[k2+1];
            uint16_t v=(c==0)?0u:((c==1)?0x5555u:0xAAAAu);
            for(int i=0;i<COLS*CSTR;i++) one[i]=v;
            show(one,cnt);
        }
        memset(prv,15,sizeof prv); build_kp();
        uint16_t *pd=malloc((size_t)NPH_GC16*COLS*CSTR*2);
        unsigned char T[256];
        double a0=now();
        for(int ph=0;ph<NPH_GC16;ph++){ prep(lut_GC16[ph],T); encode_phase2(T,pd+(size_t)ph*COLS*CSTR); }
        for(int ph=0;ph<NPH_GC16;ph++) show(pd+(size_t)ph*COLS*CSTR,1);
        memcpy(prv,img,sizeof img);
        printf("pupd: 整屏 GC16 %.0f ms\n",now()-a0);
        unsigned char TD[256]; prep(lut_DU[0],TD);
        slab_zero_all();
        const int BOXW=200,BOXH=150,BY=1420;
        int prevlo=-1, prevhi=-2;
        double te=0,tp=0;
        for(int step=0;step<NSTEP;step++){
            int bx=40+step*STEP;
            if(step>0){ int o=40+(step-1)*STEP;
                for(int y=BY;y<BY+BOXH;y++) for(int x=o;x<o+BOXW;x++)
                    if(x<UW) img[(size_t)x*UH+y]=15; }
            for(int y=BY;y<BY+BOXH;y++) for(int x=bx;x<bx+BOXW;x++)
                if(x<UW) img[(size_t)x*UH+y]=0;
            build_kp();
            int xlo=(step>0)?(bx-STEP):bx, xhi=bx+BOXW;
            int pclo=COLS-xhi, pchi=COLS-1-xlo;
            if(pclo<0)pclo=0; if(pchi>COLS-1)pchi=COLS-1;
            double a=now();
            /* 先把上一步编过的列全部清 0, 再编当前滑窗 */
            if(prevlo>=0) for(int pc=prevlo;pc<=prevhi;pc++) slab_zero_col(pc);
            for(int pc=pclo;pc<=pchi;pc++){
                const unsigned char *k=kp+(size_t)(COLS-1-pc)*UH;
                uint16_t *o=one+(size_t)pc*CSTR;
                for(int s=0;s<CSTR;s++){
                    const unsigned char *b=k+(UH-1-s*8)-7;
                    o[s]=(uint16_t)( (unsigned)TD[b[0]]      | ((unsigned)TD[b[1]]<<2)
                                   | ((unsigned)TD[b[2]]<<4) | ((unsigned)TD[b[3]]<<6)
                                   | ((unsigned)TD[b[4]]<<8) | ((unsigned)TD[b[5]]<<10)
                                   | ((unsigned)TD[b[6]]<<12)| ((unsigned)TD[b[7]]<<14) );
                }
                slab_put_col(pc,o);
            }
            slab_flush(0);
            double b2=now();
            for(int i=0;i<NF;i++) pan(0);
            double c2=now();
            te+=b2-a; tp+=c2-b2; prevlo=pclo; prevhi=pchi;
            memcpy(prv,img,sizeof img);   /* 关键: 每步同步上一帧状态, 否则旧位置不会被擦除 */
            printf("pupd: 步%d box@x=%d 脏列 %d..%d (%d/%d)  编码 %.2f ms 播放 %.2f ms 合计 %.2f ms\n",
                   step+1,bx,pclo,pchi,pchi-pclo+1,COLS,b2-a,c2-b2,c2-a);
        }
        memcpy(prv,img,sizeof img);
        printf("pupd: %d 次平均 编码 %.2f ms + 播放 %.2f ms = %.2f ms (NF=%d STEP=%d)\n",NSTEP,te/NSTEP,tp/NSTEP,(te+tp)/NSTEP,NF,STEP);
        show_blank(); show_blank();
        return 0;
    }
    if(!strcmp(m,"gscale")){
        /* 16 条竖条, 目标灰度 0..15, 走完整 GC16 -> 应看到 16 级阶梯 */
        slab_init();
        for(int x=0;x<UW;x++){ int g=x/88; if(g>15) g=15;
            memset(img+(size_t)x*UH,(unsigned char)g,UH); }
        memset(prv,15,sizeof prv);
        static uint16_t one[COLS*CSTR];
        static const int seq[]={1,24,0,4,2,24,0,5,1,24,0,4,2,24,0,2};
        for(int k=0;k<16;k+=2){
            int c=seq[k],cnt=seq[k+1];
            uint16_t v=(c==0)?0u:((c==1)?0x5555u:0xAAAAu);
            for(int i=0;i<COLS*CSTR;i++) one[i]=v;
            show(one,cnt);
        }
        printf("gscale: INIT 完成\n");
        build_kp();
        int mi=1,band=7;
        if(wbf_pool){ int tf=open("/sys/class/thermal/thermal_zone0/temp",O_RDONLY);
            char tb[32]={0}; int tc=22;
            if(tf>=0){ if(read(tf,tb,31)>0) tc=atoi(tb)/1000; close(tf);}
            band=wbf_band_for(tc); printf("gscale: 温度 %d C -> band %d\n",tc,band); }
        int nph0=0; get_phase(mi,band,0,&nph0);
        uint16_t *pd=malloc((size_t)nph0*COLS*CSTR*2);
        unsigned char T[256];
        double a=now();
        for(int ph=0;ph<nph0;ph++){ int nn=0; const unsigned char *row=get_phase(mi,band,ph,&nn);
            prep(row,T); encode_phase2(T,pd+(size_t)ph*COLS*CSTR); }
        printf("gscale: %d 相位 编码 %.0f ms\n",nph0,now()-a);
        for(int ph=0;ph<nph0;ph++){
            long c1=0,c2=0; const uint16_t *q=pd+(size_t)ph*COLS*CSTR;
            for(size_t i=0;i<(size_t)COLS*CSTR;i++){ unsigned v=q[i];
                for(int l=0;l<8;l++){ int c=(v>>(l*2))&3; if(c==1)c1++; else if(c==2)c2++; } }
            if(ph<3||ph>nph0-4) printf("gscale: ph%02d code1=%ld code2=%ld\n",ph,c1,c2);
        }
        a=now();
        for(int ph=0;ph<nph0;ph++) show(pd+(size_t)ph*COLS*CSTR,1);
        printf("gscale: 播放 %.0f ms (%.2f ms/相位)\n",now()-a,(now()-a)/nph0);
        show_blank(); show_blank();
        printf("gscale: 已补 2 帧全 0 无驱动帧(否则最后一相位会被无限重复驱动)\n");
        memcpy(prv,img,sizeof img);
        return 0;
    }
    if(!strcmp(m,"dbg")){
        if(!wbf_pool){ printf("没有载入 blob\n"); return 1; }
        printf("nband=%d nmode=%d npool=%d\n",wbf_nband,wbf_nmode,wbf_npool);
        printf("bounds:"); for(int i=0;i<=wbf_nband;i++) printf(" %d",wbf_bounds[i]); printf("\n");
        int base=1*wbf_nband+7;
        printf("nph[21](GC16 band7)=%d\n",wbf_nph[base]);
        size_t start=0; for(int i=0;i<base;i++) start+=wbf_nph[i];
        printf("start=%zu\n",start);
        for(int ph=0;ph<4;ph++){
            int nn=0; const unsigned char *row=wbf_phase(1,7,ph,&nn);
            printf("ph%d rowidx=%u nn=%d row0..15:",ph,wbf_rows[start+ph],nn);
            for(int i=0;i<16;i++) printf(" %02x",row[i]);
            printf("\n");
        }
        printf("pool[0..15]:"); for(int i=0;i<16;i++) printf(" %02x",wbf_pool[i]); printf("\n");
        return 0;
    }
    if(!strcmp(m,"blk")){
        unsigned char *buf=fbmem+0*SLOT;
        for(int r=0;r<ROWS;r++){ uint32_t*l=(uint32_t*)(buf+(size_t)r*LINE);
            for(int c=0;c<CELLS;c++) l[c]=(l[c]&0xFFFF0000u)|0x5555u; }
        for(int i=0;i<24;i++) pan(0);
        printf("blk done\n"); return 0;
    }
    make_page(); memset(prv,15,sizeof prv);

    /* --- 闪白 24 帧 (全屏 code 2)，用槽 0/1 交替 --- */
    double t0=now();
    fill_code(fbmem+0*SLOT,0xAAAAu);
    fill_code(fbmem+1*SLOT,0xAAAAu);
    for(int i=0;i<24;i++) pan((i&1)*ROWS);
    printf("闪白 24 帧: %.1f ms\n",now()-t0);
    /* 复原两槽的模板 */
    build_slot(fbmem+0*SLOT); build_slot(fbmem+1*SLOT);

    if(!strcmp(m,"verify")){
        /* 自检: 编码 phase 20，打印 pd 统计，写入 slot0 再读回比对 */
        unsigned char T[256]; prep(lut_GC16[20],T);
        uint16_t *one=malloc((size_t)COLS*CSTR*2);
        encode_phase(T,one);
        long nz=0,n1=0,n2=0;
        for(size_t i=0;i<(size_t)COLS*CSTR;i++){
            if(one[i])nz++;
            for(int l=0;l<8;l++){int c=(one[i]>>(l*2))&3; if(c==1)n1++; else if(c==2)n2++;}
        }
        printf("pd[ph20]: nz_cells=%ld code1=%ld code2=%ld (总 %d)\n",nz,n1,n2,COLS*CSTR*8);
        printf("  pc=0   pd:",0); for(int s=0;s<8;s++) printf(" %04x",one[s]); printf("\n");
        printf("  pc=700 pd:"); for(int s=0;s<8;s++) printf(" %04x",one[700*CSTR+s]); printf("\n");
        play(one,0);
        uint32_t *row0=(uint32_t*)(fbmem+0*SLOT+(size_t)4*LINE);
        uint32_t *row7=(uint32_t*)(fbmem+0*SLOT+(size_t)(4+700)*LINE);
        printf("  fb row4  cell26..29:");
        for(int s=0;s<4;s++) printf(" %08x",row0[26+s]); printf("\n");
        printf("  fb row704 cell26..29:");
        for(int s=0;s<4;s++) printf(" %08x",row7[26+s]); printf("\n");
        printf("  一致(pc0)? %s\n", ((row0[26]&0xffff)==one[0])?"YES":"NO");
        return 0;
    }
    int nph = (!strcmp(m,"fast"))?NPH_A2:NPH_GC16;
    const unsigned char (*lut)[256] = (!strcmp(m,"fast"))?lut_A2:lut_GC16;
    int n = limit>0 ? (limit<nph?limit:nph) : nph;

    uint16_t *pd=malloc((size_t)n*COLS*CSTR*2);
    if(!pd){fprintf(stderr,"nomem\n");return 1;}
    unsigned char T[256];
    t0=now();
    for(int p=0;p<n;p++){ prep(lut[p],T); encode_phase(T,pd+(size_t)p*COLS*CSTR); }
    double t1=now();
    printf("编码 %d 相位: %.1f ms (%.2f ms/相位)\n",n,t1-t0,(t1-t0)/n);

    t0=now();
    for(int p=0;p<n;p++) play(pd+(size_t)p*COLS*CSTR,p&1);
    t1=now();
    printf("播放 %d 相位: %.1f ms (%.2f ms/相位)\n",n,t1-t0,(t1-t0)/n);
    memcpy(prv,img,sizeof img);
    pan((NSLOT-1)*ROWS);
    return 0;
}
